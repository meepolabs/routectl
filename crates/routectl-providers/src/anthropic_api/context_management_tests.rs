//! Tests for the context-management thinking-cache emulation.
//! Lives as a sibling file (declared on `context_management` via
//! `#[cfg(test)] #[path = ...]`) so `context_management.rs` stays
//! under the project's 800-LOC ceiling. Tests retain access to
//! private items via `use super::*`.

use super::*;
use crate::anthropic_api::types::{
    AnthropicContent, AnthropicMessage, AnthropicRole, ContentBlock,
};
use routectl_core::{ReasoningDetail, ReasoningDetailKind};
use std::num::NonZeroUsize;
use std::sync::RwLock;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------
// extract_tool_thinking helpers
// ------------------------------------------------------------------

fn thinking_block(thinking: &str, signature: &str) -> ContentBlock {
    ContentBlock::Thinking {
        thinking: thinking.to_string(),
        signature: signature.to_string(),
        cache_control: None,
    }
}

fn tool_use_block(id: &str) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: "tool".to_string(),
        input: serde_json::Value::Object(serde_json::Map::new()),
        cache_control: None,
    }
}

fn text_block(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
        citations: None,
        cache_control: None,
    }
}

// ------------------------------------------------------------------
// extract_tool_thinking tests (RED -- function absent until below)
// ------------------------------------------------------------------

/// [Thinking, ToolUse] -> 1 entry carrying the preceding thinking.
#[test]
fn extract_thinking_before_tool_use() {
    let blocks = vec![thinking_block("hello", "sig1"), tool_use_block("id1")];
    let result = extract_tool_thinking(&blocks);
    assert_eq!(result.len(), 1);
    let (id, details) = &result[0];
    assert_eq!(id, "id1");
    assert_eq!(details.len(), 1);
    assert_eq!(details[0].payload["text"], "hello");
    assert_eq!(details[0].payload["signature"], "sig1");
    assert!(matches!(details[0].kind, ReasoningDetailKind::Text));
}

/// [Thinking1, Thinking2, ToolUse] -> 1 entry with both details.
#[test]
fn extract_two_thinking_one_tool_use() {
    let blocks = vec![
        thinking_block("step1", "s1"),
        thinking_block("step2", "s2"),
        tool_use_block("id1"),
    ];
    let result = extract_tool_thinking(&blocks);
    assert_eq!(result.len(), 1);
    let (id, details) = &result[0];
    assert_eq!(id, "id1");
    assert_eq!(details.len(), 2);
    assert_eq!(details[0].payload["text"], "step1");
    assert_eq!(details[1].payload["text"], "step2");
}

/// [Thinking1, ToolUse1, Thinking2, ToolUse2] -> 2 entries;
/// each entry carries ONLY the thinking that immediately preceded it
/// (non-cumulative -- running vec is reset after each emission).
#[test]
fn extract_two_tool_uses_cumulative() {
    let blocks = vec![
        thinking_block("t1", "sig1"),
        tool_use_block("tool1"),
        thinking_block("t2", "sig2"),
        tool_use_block("tool2"),
    ];
    let result = extract_tool_thinking(&blocks);
    assert_eq!(result.len(), 2);
    let (id1, details1) = &result[0];
    assert_eq!(id1, "tool1");
    assert_eq!(details1.len(), 1);
    assert_eq!(details1[0].payload["text"], "t1");
    let (id2, details2) = &result[1];
    assert_eq!(id2, "tool2");
    // Non-cumulative: tool2 sees only its immediately preceding block (t2).
    assert_eq!(details2.len(), 1);
    assert_eq!(details2[0].payload["text"], "t2");
}

/// [Thinking, Text] -> empty (no tool_use, nothing emitted).
#[test]
fn extract_no_tool_use_returns_empty() {
    let blocks = vec![thinking_block("step1", "sig1"), text_block("some text")];
    let result = extract_tool_thinking(&blocks);
    assert!(result.is_empty(), "expected empty result");
}

/// [] -> empty.
#[test]
fn extract_empty_blocks_returns_empty() {
    let result = extract_tool_thinking(&[]);
    assert!(result.is_empty(), "expected empty result for empty input");
}

/// [Text, ToolUse] -> 1 entry with empty thinking vec (valid no-op
/// for the inject path which just skips an empty list).
#[test]
fn extract_text_then_tool_use_emits_empty_thinking() {
    let blocks = vec![text_block("content"), tool_use_block("id1")];
    let result = extract_tool_thinking(&blocks);
    assert_eq!(result.len(), 1);
    let (id, details) = &result[0];
    assert_eq!(id, "id1");
    assert!(details.is_empty(), "expected empty thinking vec");
}

/// [Thinking, ToolUse{id:""}] -> empty (empty id is skipped).
#[test]
fn extract_skips_tool_use_with_empty_id() {
    let blocks = vec![thinking_block("hello", "sig1"), tool_use_block("")];
    let result = extract_tool_thinking(&blocks);
    assert!(
        result.is_empty(),
        "expected empty result for empty tool_use id"
    );
}

fn make_thinking(text: &str) -> Vec<ReasoningDetail> {
    vec![ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("test-id".into()),
        format: Some(super::super::ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: serde_json::json!({"text": text, "signature": "sig"}),
    }]
}

fn small_cache(cap: usize) -> RwLock<ThinkingCache> {
    RwLock::new(lru::LruCache::new(NonZeroUsize::new(cap).expect("cap > 0")))
}

/// Test helper: call `snapshot_to_cache` with the default per-entry
/// byte cap. Existing tests pre-date the cap parameter and don't
/// care about it; this wrapper keeps each call site one line.
fn snap(
    cache: &RwLock<ThinkingCache>,
    provider_id: &str,
    tool_use_id: &str,
    thinking: Vec<ReasoningDetail>,
) {
    snapshot_to_cache(
        cache,
        provider_id,
        tool_use_id,
        thinking,
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        crate::anthropic_api::context_management::THINKING_CACHE_TTL,
        "test",
    );
}

/// snapshot_to_cache followed by lookup_thinking with the same key
/// must return Some with the originally stored thinking vec.
#[test]
fn insert_and_lookup_hit() {
    let cache = small_cache(4);
    let thinking = make_thinking("hello world");
    snap(&cache, "provider-a", "tool-1", thinking.clone());
    let result = lookup_thinking(&cache, "provider-a", "tool-1");
    assert!(result.is_some(), "expected Some but got None");
    let got = result.unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, thinking[0].id);
}

/// Lookup with a key that was never inserted must return None.
#[test]
fn lookup_miss_unknown_key() {
    let cache = small_cache(4);
    let result = lookup_thinking(&cache, "provider-a", "tool-unknown");
    assert!(result.is_none(), "expected None for unknown key");
}

/// An entry whose expires_at is in the past must be treated as stale
/// and lookup must return None. The entry is inserted directly (bypassing
/// snapshot_to_cache) so the test can control the expiry timestamp.
#[test]
fn ttl_expiry_returns_none() {
    let cache = small_cache(4);
    let key = ("provider-b".to_string(), "tool-old".to_string());
    let entry = ThinkingCacheEntry {
        thinking: make_thinking("stale"),
        expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        ttl: Duration::from_hours(1),
    };
    cache.write().expect("lock").put(key, entry);
    let result = lookup_thinking(&cache, "provider-b", "tool-old");
    assert!(result.is_none(), "expected None for expired entry");
}

/// LOW-3 fix: looking up an expired entry must EVICT it, not merely return
/// None. Before the fix, `lookup_thinking` used `get_mut` (which promotes
/// to MRU) and returned None on expiry without removing the entry --
/// leaving a dead entry occupying an MRU slot. After the fix the expired
/// entry is popped from the cache, so `len()` drops and a subsequent
/// `peek` returns None.
#[test]
fn lookup_evicts_expired_entry() {
    let cache = small_cache(4);
    let key = ("provider-c".to_string(), "tool-stale".to_string());
    let entry = ThinkingCacheEntry {
        thinking: make_thinking("stale"),
        expires_at: Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
        ttl: Duration::from_hours(1),
    };
    cache.write().expect("lock").put(key.clone(), entry);
    assert_eq!(
        cache.read().expect("lock").len(),
        1,
        "entry present before lookup"
    );

    // Act: lookup an expired entry.
    let result = lookup_thinking(&cache, "provider-c", "tool-stale");

    // Assert: None returned AND the entry is gone from the cache.
    assert!(result.is_none(), "expired lookup must return None");
    assert_eq!(
        cache.read().expect("lock").len(),
        0,
        "expired entry must be evicted on lookup, not left occupying a slot"
    );
    assert!(
        cache.write().expect("lock").peek(&key).is_none(),
        "subsequent peek of the evicted key must return None"
    );
}

/// When the cache is full (cap = N) and one more entry is inserted,
/// the LRU entry (the first one inserted) must be evicted. The most
/// recent N entries must still be present.
#[test]
fn capacity_eviction_drops_lru() {
    const SMALL_CAP: usize = 4;
    let cache = small_cache(SMALL_CAP);
    for i in 0..=(SMALL_CAP) {
        snap(
            &cache,
            "prov",
            &format!("tool-{i}"),
            make_thinking(&format!("thinking-{i}")),
        );
    }
    // tool-0 was LRU; it must have been evicted.
    assert!(
        lookup_thinking(&cache, "prov", "tool-0").is_none(),
        "first inserted entry should have been evicted"
    );
    // The most recent SMALL_CAP entries must still be present.
    for i in 1..=(SMALL_CAP) {
        assert!(
            lookup_thinking(&cache, "prov", &format!("tool-{i}")).is_some(),
            "entry tool-{i} should still be in cache"
        );
    }
}

/// Snapshotting the same key twice must overwrite the first entry;
/// lookup must return the second (most recent) thinking vec.
#[test]
fn idempotent_reinsert_replaces() {
    let cache = small_cache(4);
    let first = make_thinking("first-thinking");
    let second = make_thinking("second-thinking");
    snap(&cache, "prov", "tool-x", first);
    snap(&cache, "prov", "tool-x", second);
    let result =
        lookup_thinking(&cache, "prov", "tool-x").expect("entry should exist after second insert");
    assert_eq!(
        result[0].payload["text"], "second-thinking",
        "second insert must replace first; got {:?}",
        result[0].payload
    );
}

// ------------------------------------------------------------------
// apply_clear_thinking_edit helpers
// ------------------------------------------------------------------

fn assistant_with_tool(tool_id: &str) -> AnthropicMessage {
    AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: AnthropicContent::Blocks(vec![tool_use_block(tool_id)]),
    }
}

fn extras_keep_all() -> serde_json::Value {
    serde_json::json!({
        "context_management": {
            "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
        }
    })
}

fn extras_keep_last_n(n: u64) -> serde_json::Value {
    serde_json::json!({
        "context_management": {
            "edits": [{
                "type": CLEAR_THINKING_EDIT_TYPE,
                "keep": {"type": "thinking_turns", "value": n}
            }]
        }
    })
}

fn extras_unknown_keep() -> serde_json::Value {
    serde_json::json!({
        "context_management": {
            "edits": [{
                "type": CLEAR_THINKING_EDIT_TYPE,
                "keep": {"type": "not_a_real_type", "value": 99}
            }]
        }
    })
}

fn seed_cache(cache: &RwLock<ThinkingCache>, tool_id: &str) {
    snap(cache, "prov", tool_id, make_thinking("my-thinking"));
}

fn blocks_of(msg: &AnthropicMessage) -> &Vec<ContentBlock> {
    match &msg.content {
        AnthropicContent::Blocks(b) => b,
        AnthropicContent::Text(_) => panic!("expected Blocks"),
    }
}

// ------------------------------------------------------------------
// apply_clear_thinking_edit tests (1-8)
// ------------------------------------------------------------------

/// keep="all", one assistant message with ToolUse, cache populated.
/// Thinking block must be injected immediately before the ToolUse block.
#[test]
fn apply_edit_keep_all_injects_thinking() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = extras_keep_all();
    let mut messages = vec![assistant_with_tool("t1")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty(), "no misses expected");
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 2, "must have [Thinking, ToolUse]");
    assert!(
        matches!(&blocks[0], ContentBlock::Thinking { thinking, .. } if thinking == "my-thinking"),
        "first block must be the injected Thinking"
    );
    assert!(matches!(&blocks[1], ContentBlock::ToolUse { id, .. } if id == "t1"));
}

/// keep=LastN(1), two assistant messages (t1, t2). Only the second
/// message (t2) must receive injection; the first (t1) must be untouched.
#[test]
fn apply_edit_keep_last_1_of_2_only_last_injected() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    seed_cache(&cache, "t2");
    let extras = extras_keep_last_n(1);
    let mut messages = vec![assistant_with_tool("t1"), assistant_with_tool("t2")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty());
    // messages[0] (t1) must be untouched -- still just [ToolUse].
    let blocks0 = blocks_of(&messages[0]);
    assert_eq!(blocks0.len(), 1, "first message must stay as [ToolUse]");
    // messages[1] (t2) must have [Thinking, ToolUse].
    let blocks1 = blocks_of(&messages[1]);
    assert_eq!(
        blocks1.len(),
        2,
        "second message must be [Thinking, ToolUse]"
    );
    assert!(matches!(&blocks1[0], ContentBlock::Thinking { .. }));
}

/// keep={"type":"thinking_turns","value":0} means KeepPolicy::None.
/// No injection must occur.
#[test]
fn apply_edit_keep_0_no_injection() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = extras_keep_last_n(0);
    let mut messages = vec![assistant_with_tool("t1")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(
        result.missed_tool_ids.is_empty(),
        "no misses expected with keep=0 (no injection attempted)"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 1, "message must remain [ToolUse]");
}

/// Unknown keep shape must default to KeepPolicy::All and inject.
#[test]
fn apply_edit_unknown_keep_defaults_to_all() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = extras_unknown_keep();
    let mut messages = vec![assistant_with_tool("t1")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(
        result.missed_tool_ids.is_empty(),
        "unknown keep must fall back to All; no misses expected"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 2, "must be [Thinking, ToolUse]");
}

/// Cache miss: tool_id not in cache. missed_tool_ids must contain the id;
/// thinking_injected must be false; message must be untouched.
#[test]
fn apply_edit_cache_miss_returned_in_result() {
    let cache = small_cache(8); // nothing seeded
    let extras = extras_keep_all();
    let mut messages = vec![assistant_with_tool("t99")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert_eq!(result.missed_tool_ids, vec!["t99".to_string()]);
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 1, "message must remain [ToolUse] on miss");
}

/// Idempotency: message already has [Thinking, ToolUse]. The guard
/// (checks whether the preceding block is already a Thinking block)
/// fires before the cache lookup and prevents a second injection.
/// The cache entry is irrelevant here -- the guard is the gate.
#[test]
fn apply_edit_skips_when_thinking_already_precedes_tool_use() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = extras_keep_all();
    // Pre-populate message with thinking already in place.
    let mut messages = vec![AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: AnthropicContent::Blocks(vec![
            thinking_block("already-there", "sig"),
            tool_use_block("t1"),
        ]),
    }];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    // The guard prevented injection (thinking already precedes tool_use).
    // Because the guard fires before the cache lookup the miss vec is also
    // empty -- no lookup was attempted.
    assert!(
        result.missed_tool_ids.is_empty(),
        "guard must prevent injection and not record a miss"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(
        blocks.len(),
        2,
        "must remain [Thinking, ToolUse] without doubling"
    );
}

/// No CLEAR_THINKING_EDIT_TYPE edit in extras -> function is a no-op.
#[test]
fn apply_edit_no_clear_thinking_edit_is_noop() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = serde_json::json!({
        "context_management": {
            "edits": [{"type": "some_other_edit"}]
        }
    });
    let mut messages = vec![assistant_with_tool("t1")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty());
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 1, "message must be untouched");
}

/// Empty messages vec -> no-op, returns missed_tool_ids empty.
#[test]
fn apply_edit_empty_messages_is_noop() {
    let cache = small_cache(8);
    let extras = extras_keep_all();
    let mut messages: Vec<AnthropicMessage> = vec![];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty());
    assert!(messages.is_empty());
}

// ------------------------------------------------------------------
// reasoning_detail_to_thinking_block guard tests (fix 3)
// ------------------------------------------------------------------

fn make_detail(
    kind: ReasoningDetailKind,
    format: Option<&str>,
    payload: serde_json::Value,
) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: Some("test-rd".into()),
        format: format.map(std::string::ToString::to_string),
        index: Some(0),
        payload,
    }
}

/// Text detail with empty signature must produce None.
/// An empty-signature Thinking block 400s on real Anthropic API.
#[test]
fn inject_skips_text_with_empty_signature() {
    let cache = small_cache(8);
    let detail = make_detail(
        ReasoningDetailKind::Text,
        Some(super::super::ANTHROPIC_FORMAT),
        serde_json::json!({"text": "some thinking", "signature": ""}),
    );
    snap(&cache, "prov", "t-empty-sig", vec![detail]);
    let extras = extras_keep_all();
    let mut messages = vec![assistant_with_tool("t-empty-sig")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    // Cache hit but filtered out; no injection -> treated as a miss.
    assert_eq!(
        result.missed_tool_ids,
        vec!["t-empty-sig".to_string()],
        "empty-signature detail must be treated as a miss (no block injected)"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(
        blocks.len(),
        1,
        "no Thinking block must be injected for empty-signature detail"
    );
}

/// Text detail with a format other than `anthropic-claude-v1` must produce None.
#[test]
fn inject_skips_text_with_wrong_format() {
    let cache = small_cache(8);
    let detail = make_detail(
        ReasoningDetailKind::Text,
        Some("some-other-format"),
        serde_json::json!({"text": "some thinking", "signature": "sig-valid"}),
    );
    snap(&cache, "prov", "t-wrong-fmt", vec![detail]);
    let extras = extras_keep_all();
    let mut messages = vec![assistant_with_tool("t-wrong-fmt")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert_eq!(
        result.missed_tool_ids,
        vec!["t-wrong-fmt".to_string()],
        "wrong-format detail must be treated as a miss"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(blocks.len(), 1, "no Thinking block must be injected");
}

/// Summary kind must map to None; no block injected.
#[test]
fn reasoning_detail_to_thinking_block_skips_summary() {
    let detail = make_detail(
        ReasoningDetailKind::Summary,
        Some(super::super::ANTHROPIC_FORMAT),
        serde_json::json!({"text": "summary text"}),
    );
    let result = reasoning_detail_to_thinking_block(&detail);
    assert!(
        result.is_none(),
        "Summary kind must map to None; got: {result:?}"
    );
}

// ------------------------------------------------------------------
// Fix 1: non-cumulative interleaved-thinking tests
// ------------------------------------------------------------------

/// Non-cumulative invariant (non-streaming path):
/// [ThinkingA, ToolUse1, ThinkingB, ToolUse2] -> tool1 cached as [A]
/// only, tool2 cached as [B] only (no duplicate A in tool2's vec).
#[test]
fn extract_interleaved_thinking_non_cumulative() {
    let blocks = vec![
        thinking_block("alpha", "sa"),
        tool_use_block("t1"),
        thinking_block("beta", "sb"),
        tool_use_block("t2"),
    ];
    let result = extract_tool_thinking(&blocks);
    assert_eq!(result.len(), 2);
    let entry_t1 = result
        .iter()
        .find(|(id, _)| id == "t1")
        .expect("t1 missing");
    let entry_t2 = result
        .iter()
        .find(|(id, _)| id == "t2")
        .expect("t2 missing");
    assert_eq!(entry_t1.1.len(), 1);
    assert_eq!(entry_t1.1[0].payload["text"], "alpha");
    assert_eq!(
        entry_t2.1.len(),
        1,
        "t2 must see only its own thinking (not cumulative)"
    );
    assert_eq!(entry_t2.1[0].payload["text"], "beta");
}

/// Inject side: cache has tool1->[A], tool2->[B].
/// Outgoing message [ToolUse1, ToolUse2] becomes [A, ToolUse1, B, ToolUse2].
/// No duplicate A appears before ToolUse2.
#[test]
fn apply_edit_two_tool_uses_no_duplicate_thinking() {
    let cache = small_cache(8);
    snap(&cache, "prov", "tu1", make_thinking("think-a"));
    snap(&cache, "prov", "tu2", make_thinking("think-b"));
    let extras = extras_keep_all();
    let mut messages = vec![AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: AnthropicContent::Blocks(vec![tool_use_block("tu1"), tool_use_block("tu2")]),
    }];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty(), "no misses expected");
    let blocks = blocks_of(&messages[0]);
    // Expected: [ThinkA, ToolUse1, ThinkB, ToolUse2]
    assert_eq!(
        blocks.len(),
        4,
        "must be [ThinkA, ToolUse1, ThinkB, ToolUse2]"
    );
    assert!(matches!(&blocks[0], ContentBlock::Thinking { thinking, .. } if thinking == "think-a"));
    assert!(matches!(&blocks[1], ContentBlock::ToolUse { id, .. } if id == "tu1"));
    assert!(
        matches!(&blocks[2], ContentBlock::Thinking { thinking, .. } if thinking == "think-b"),
        "think-b must precede tu2, not think-a"
    );
    assert!(matches!(&blocks[3], ContentBlock::ToolUse { id, .. } if id == "tu2"));
}

// ------------------------------------------------------------------
// Fix 2: Some([]) vs None distinction test
// ------------------------------------------------------------------

/// Some([]) must NOT be treated as a cache miss. A tool_use that was
/// preceded by no thinking has an empty vec in the cache (Some([]));
/// this is a successful lookup -- nothing to inject, not a miss.
/// Contrast with None (no cache entry) which IS a miss.
///
/// Mixed turn: tool1 has [A] (non-empty), tool2 has [] (empty Some).
/// Expected: tool1 gets A injected, tool2 untouched, no misses.
#[test]
fn apply_edit_some_empty_not_treated_as_miss() {
    let cache = small_cache(8);
    snap(&cache, "prov", "tA", make_thinking("think-alpha"));
    // Empty vec: some-but-nothing-to-inject.
    snap(&cache, "prov", "tB", vec![]);
    let extras = extras_keep_all();
    let mut messages = vec![AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: AnthropicContent::Blocks(vec![tool_use_block("tA"), tool_use_block("tB")]),
    }];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(
        result.missed_tool_ids.is_empty(),
        "Some([]) must not be treated as a miss; got: {:?}",
        result.missed_tool_ids
    );
    let blocks = blocks_of(&messages[0]);
    // Expected: [ThinkAlpha, ToolUseA, ToolUseB] (tB unchanged)
    assert_eq!(blocks.len(), 3, "must be [Thinking, ToolUseA, ToolUseB]");
    assert!(
        matches!(&blocks[0], ContentBlock::Thinking { thinking, .. } if thinking == "think-alpha")
    );
    assert!(matches!(&blocks[1], ContentBlock::ToolUse { id, .. } if id == "tA"));
    assert!(matches!(&blocks[2], ContentBlock::ToolUse { id, .. } if id == "tB"));
}

// ------------------------------------------------------------------
// Fix 3: cold-cache idempotency test
// ------------------------------------------------------------------

/// Cold-cache idempotency: empty cache, but message already has
/// [Thinking, ToolUse]. The idempotency guard fires before the cache
/// lookup, so no cache miss is recorded (the lookup is never attempted).
#[test]
fn apply_edit_cold_cache_idempotency() {
    let cache = small_cache(8); // empty
    let extras = extras_keep_all();
    let mut messages = vec![AnthropicMessage {
        role: AnthropicRole::Assistant,
        content: AnthropicContent::Blocks(vec![
            thinking_block("pre-existing", "sig"),
            tool_use_block("cold-tu"),
        ]),
    }];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    // Guard fired before cache lookup -> no miss, block count unchanged.
    assert!(
        result.missed_tool_ids.is_empty(),
        "cold-cache + idempotency guard must not record a miss; got: {:?}",
        result.missed_tool_ids
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(
        blocks.len(),
        2,
        "block count must remain [Thinking, ToolUse]"
    );
}

// ------------------------------------------------------------------
// Fix 8: bare-number keep value tests
// ------------------------------------------------------------------

fn extras_bare_number_keep(n: u64) -> serde_json::Value {
    serde_json::json!({
        "context_management": {
            "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": n}]
        }
    })
}

/// keep = bare 2 -> LastN(2). With two qualifying assistant messages,
/// both must be injected.
#[test]
fn apply_edit_keep_bare_number_handled_as_lastn() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    seed_cache(&cache, "t2");
    let extras = extras_bare_number_keep(2);
    let mut messages = vec![assistant_with_tool("t1"), assistant_with_tool("t2")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(result.missed_tool_ids.is_empty());
    // Both messages must have [Thinking, ToolUse].
    assert_eq!(blocks_of(&messages[0]).len(), 2, "t1 must be injected");
    assert_eq!(blocks_of(&messages[1]).len(), 2, "t2 must be injected");
}

/// keep = bare 0 -> None. No injection must occur; no misses.
#[test]
fn apply_edit_keep_bare_zero_handled_as_none() {
    let cache = small_cache(8);
    seed_cache(&cache, "t1");
    let extras = extras_bare_number_keep(0);
    let mut messages = vec![assistant_with_tool("t1")];

    let result = apply_clear_thinking_edit(&mut messages, Some(&extras), &cache, "prov");

    assert!(
        result.missed_tool_ids.is_empty(),
        "bare zero must be KeepPolicy::None; no injection attempted, no miss"
    );
    let blocks = blocks_of(&messages[0]);
    assert_eq!(
        blocks.len(),
        1,
        "must remain [ToolUse] -- no injection for keep=0"
    );
}

// ------------------------------------------------------------------
// Per-entry byte-cap tests
// ------------------------------------------------------------------

/// Build a thinking detail vec whose serialized JSON is approximately
/// `payload_bytes` long. We pad the `text` field with `'a'` characters
/// to control the size; the surrounding wire envelope adds ~120 bytes
/// of overhead which is negligible at the sizes we test against.
fn make_thinking_of_size(payload_bytes: usize) -> Vec<ReasoningDetail> {
    let padding = "a".repeat(payload_bytes);
    vec![ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("rd-cap".into()),
        format: Some(super::super::ANTHROPIC_FORMAT.into()),
        index: Some(0),
        payload: serde_json::json!({"text": padding, "signature": "sig"}),
    }]
}

/// Under-cap entry must be inserted normally and round-trip via
/// `lookup_thinking`.
#[test]
fn snapshot_to_cache_under_cap_inserts_and_round_trips() {
    let cache = small_cache(4);
    // ~512 KB payload, well under the 1 MiB default cap.
    let thinking = make_thinking_of_size(512 * 1024);
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-under",
        thinking.clone(),
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        crate::anthropic_api::context_management::THINKING_CACHE_TTL,
        "complete",
    );
    let got =
        lookup_thinking(&cache, "prov", "tool-under").expect("under-cap entry must round-trip");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].id, thinking[0].id);
}

/// Over-cap entry must be rejected: the LRU is unchanged and the
/// next lookup misses.
#[test]
fn snapshot_to_cache_over_cap_is_rejected() {
    let cache = small_cache(4);
    // ~1.5 MiB payload, well over the 1 MiB default cap.
    let thinking = make_thinking_of_size(1536 * 1024);
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-over",
        thinking,
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        crate::anthropic_api::context_management::THINKING_CACHE_TTL,
        "stream",
    );
    assert!(
        lookup_thinking(&cache, "prov", "tool-over").is_none(),
        "over-cap entry must be rejected; lookup should miss"
    );
}

/// `snapshot_to_cache` accepts a per-call cap argument; it must be
/// honored even when the caller picks a value below the hardcoded
/// default. A 1024-byte cap rejects a 2 KB entry that would
/// otherwise pass under the default.
#[test]
fn snapshot_to_cache_honors_per_call_cap_override() {
    let cache = small_cache(4);
    let thinking = make_thinking_of_size(2 * 1024);
    let default_ttl = crate::anthropic_api::context_management::THINKING_CACHE_TTL;
    // 2 KB entry is well under the 1 MiB default cap...
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-default",
        thinking.clone(),
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        default_ttl,
        "complete",
    );
    assert!(
        lookup_thinking(&cache, "prov", "tool-default").is_some(),
        "2 KB entry must round-trip under the default cap"
    );
    // ...but rejected under a tightened 1024-byte cap.
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-tight",
        thinking,
        1024,
        default_ttl,
        "complete",
    );
    assert!(
        lookup_thinking(&cache, "prov", "tool-tight").is_none(),
        "2 KB entry must be rejected when the per-call cap is 1 KB"
    );
}

/// `snapshot_to_cache` must honor the caller-supplied TTL: an entry
/// written with a sub-second TTL must miss on lookup after a short
/// sleep.
#[test]
fn snapshot_to_cache_honors_per_call_ttl_override() {
    let cache = small_cache(4);
    let thinking = make_thinking("ttl-test");
    let short_ttl = std::time::Duration::from_millis(50);
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-ttl",
        thinking,
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        short_ttl,
        "complete",
    );
    // Sleep past the TTL window so the entry expires.
    std::thread::sleep(std::time::Duration::from_millis(120));
    assert!(
        lookup_thinking(&cache, "prov", "tool-ttl").is_none(),
        "entry must be treated as stale after its TTL expires"
    );
}

/// Sliding TTL: every successful hit refreshes `expires_at` to
/// `now + ttl`. An entry stays alive across multiple lookups as long
/// as the gap between any two consecutive hits stays under the TTL,
/// even when the cumulative wall-clock since insert exceeds it.
#[test]
fn lookup_thinking_refreshes_expires_at_on_hit() {
    let cache = small_cache(4);
    let thinking = make_thinking("sliding");
    let ttl = std::time::Duration::from_millis(500);
    snapshot_to_cache(
        &cache,
        "prov",
        "tool-slide",
        thinking,
        DEFAULT_MAX_THINKING_ENTRY_BYTES,
        ttl,
        "complete",
    );

    // ~200ms in: still within the original TTL window. Hit must refresh.
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(
        lookup_thinking(&cache, "prov", "tool-slide").is_some(),
        "first hit at ~200ms must succeed"
    );

    // ~300ms after the first hit: within the refreshed window even
    // though total wall-clock since insert is ~500ms (at the 500ms
    // original TTL boundary). With sliding semantics the entry is
    // still alive because the first hit pushed expires_at out by
    // another full TTL.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        lookup_thinking(&cache, "prov", "tool-slide").is_some(),
        "sliding TTL: second hit must succeed because the first hit refreshed expires_at"
    );
}

/// The thinking-cache LRU bounds memory by capacity: writing
/// `cap + 1` distinct entries must evict the oldest.
#[test]
fn cache_capacity_honors_per_provider_entries_cap() {
    // Arrange: a tiny cap of 2.
    let cache = small_cache(2);
    let default_ttl = crate::anthropic_api::context_management::THINKING_CACHE_TTL;
    for i in 0..3 {
        snapshot_to_cache(
            &cache,
            "prov",
            &format!("tool-{i}"),
            make_thinking(&format!("thinking-{i}")),
            DEFAULT_MAX_THINKING_ENTRY_BYTES,
            default_ttl,
            "complete",
        );
    }
    // Assert: tool-0 was LRU; the cap-2 cache evicted it on the third
    // write. The two newest remain.
    assert!(
        lookup_thinking(&cache, "prov", "tool-0").is_none(),
        "entry 0 must be evicted under a cap-of-2"
    );
    assert!(
        lookup_thinking(&cache, "prov", "tool-1").is_some(),
        "entry 1 must remain"
    );
    assert!(
        lookup_thinking(&cache, "prov", "tool-2").is_some(),
        "entry 2 must remain"
    );
}

// ------------------------------------------------------------------
// Shared ReasoningDetail constructor tests
// ------------------------------------------------------------------

/// `make_thinking_detail` must produce a `Text`-kind detail whose
/// payload carries the full `(text, signature)` pair under the
/// Anthropic format tag. This pins the wire shape both the
/// non-streaming (`extract_tool_thinking`) and streaming
/// (`sse.rs` aggregated terminal) paths emit so a future drift
/// produces a test failure rather than silent divergence.
#[test]
fn make_thinking_detail_pins_shape() {
    let detail = make_thinking_detail(
        "fixed-id".to_string(),
        7,
        "the thinking text".to_string(),
        "the-signature".to_string(),
    );
    assert!(matches!(detail.kind, ReasoningDetailKind::Text));
    assert_eq!(detail.id.as_deref(), Some("fixed-id"));
    assert_eq!(
        detail.format.as_deref(),
        Some(super::super::ANTHROPIC_FORMAT)
    );
    assert_eq!(detail.index, Some(7));
    assert_eq!(detail.payload["text"], "the thinking text");
    assert_eq!(detail.payload["signature"], "the-signature");
}

/// Empty signature must serialize as the empty string (not null);
/// this is the streaming-aggregated branch's behaviour and the
/// non-streaming path tolerates either shape.
#[test]
fn make_thinking_detail_empty_signature_is_empty_string() {
    let detail = make_thinking_detail("id".to_string(), 0, "t".to_string(), String::new());
    assert_eq!(detail.payload["signature"], "");
}

/// `make_redacted_thinking_detail` must produce an `Encrypted`-kind
/// detail whose payload carries the opaque `data` field only. Pins
/// the wire shape for both the non-streaming and streaming
/// `redacted_thinking` emission paths.
#[test]
fn make_redacted_thinking_detail_pins_shape() {
    let detail =
        make_redacted_thinking_detail("redacted-id".to_string(), 3, "opaque-blob".to_string());
    assert!(matches!(detail.kind, ReasoningDetailKind::Encrypted));
    assert_eq!(detail.id.as_deref(), Some("redacted-id"));
    assert_eq!(
        detail.format.as_deref(),
        Some(super::super::ANTHROPIC_FORMAT)
    );
    assert_eq!(detail.index, Some(3));
    assert_eq!(detail.payload["data"], "opaque-blob");
}
