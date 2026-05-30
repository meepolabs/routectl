//! Server-side emulation of Anthropic's context-management-2025-06-27 beta
//! for non-Anthropic anthropic-api providers. Stores thinking blocks observed
//! in upstream responses for re-injection on next-turn requests where
//! claude-code stripped them under the beta. See docs/PROVIDER-QUIRKS.md for
//! the operator-level explanation.

use routectl_core::{ReasoningDetail, ReasoningDetailKind};

/// Beta flag that enables Anthropic's server-side context-management.
/// Stripped from outgoing headers when emulation mode is active.
pub(crate) const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Edit type tag for the thinking-strip edit in context_management edits arrays.
pub(crate) const CLEAR_THINKING_EDIT_TYPE: &str = "clear_thinking_20251015";

/// Cache key: `(provider_id, tool_use_id)`.
/// The provider_id scope ensures that two providers sharing the same
/// tool_use_id (unlikely but possible under multi-provider configs) never
/// cross-contaminate each other's thinking stores.
pub(crate) type ThinkingCacheKey = (String, String);

/// A single cached thinking observation.
pub(crate) struct ThinkingCacheEntry {
    /// The reasoning blocks captured from the upstream response that
    /// followed the tool_use block identified by the cache key.
    pub(crate) thinking: Vec<ReasoningDetail>,
    /// Wall-clock expiry. The store evicts entries that are past this
    /// instant on the next access attempt (checked by the reader).
    pub(crate) expires_at: std::time::Instant,
}

/// LRU map from `(provider_id, tool_use_id)` to a thinking observation.
/// Bounded at `THINKING_CACHE_CAP` entries; oldest entries are evicted
/// when the cap is reached (standard LRU semantics).
pub(crate) type ThinkingCache = lru::LruCache<ThinkingCacheKey, ThinkingCacheEntry>;

/// Maximum number of `(provider_id, tool_use_id)` pairs held in the cache
/// at once. Covers ~1 000 concurrent in-flight tool turns before the LRU
/// starts evicting the oldest; this is generous for a single-process proxy.
pub(crate) const THINKING_CACHE_CAP: usize = 1000;

/// TTL for cached thinking entries. Entries older than 60 minutes are
/// treated as stale and discarded on next read.
/// 60 minutes matches the typical maximum agentic session length before
/// context rotation.
pub(crate) const THINKING_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Default per-entry byte cap on the thinking cache. The cap is applied
/// at write time -- entries whose serialized JSON byte length exceeds
/// this value are rejected (NOT truncated) so the rest of the pipeline's
/// cache-miss recovery (request.rs strip-thinking-on-miss) handles them
/// the same way as a TTL eviction.
///
/// Truncation would corrupt the opaque continuity signature on Anthropic
/// thinking blocks; rejecting and letting the strip-on-miss path land
/// the request without thinking is the behaviour-preserving choice.
///
/// Provider-configurable via `[providers.X] max_thinking_entry_bytes`
/// (anthropic-api kind only). 256 KB is generous for ordinary agent
/// thinking turns (typical sizes are 1-50 KB) while bounding the
/// LRU's worst-case memory use to `THINKING_CACHE_CAP * cap`
/// (1000 * 256 KB = ~256 MB ceiling) under adversarial inputs.
pub(crate) const MAX_THINKING_ENTRY_BYTES: usize = 256 * 1024;

/// Store a thinking observation into the cache under `(provider_id, tool_use_id)`.
/// Overwrites any existing entry for the same key.
///
/// Rejects writes whose serialized JSON byte length exceeds
/// `max_entry_bytes`. The serialization round-trip is cheap relative
/// to the cache write itself and captures every payload field
/// (text + signature + data). On rejection the LRU is NOT touched and
/// a structured WARN is emitted so operators can grep for oversized
/// inputs. `path` tags the call site ("complete" / "stream") in the
/// log.
pub(crate) fn snapshot_to_cache(
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    tool_use_id: &str,
    thinking: Vec<routectl_core::ReasoningDetail>,
    max_entry_bytes: usize,
    path: &'static str,
) {
    // Measure once. A failure here would mean serde_json couldn't
    // serialize a Vec<ReasoningDetail>, which the type's serde-derived
    // impls rule out. Entries whose size cannot be measured are accepted
    // (fail-open) to avoid spurious cache-miss storms on a future serde
    // regression. The trade-off is that such a regression would silently
    // disable the cap for affected entries; if that becomes a concern,
    // a fallback measurement path (e.g. summing payload byte lengths)
    // can be added.
    let observed_bytes = serde_json::to_vec(&thinking).map(|v| v.len()).unwrap_or(0);
    if observed_bytes > max_entry_bytes {
        tracing::warn!(
            provider = %provider_id,
            tool_use_id,
            observed_bytes,
            cap_bytes = max_entry_bytes,
            detail_count = thinking.len(),
            path,
            "thinking-cache entry exceeds per-entry byte cap; rejecting write"
        );
        return;
    }
    let key = (provider_id.to_string(), tool_use_id.to_string());
    let entry = ThinkingCacheEntry {
        thinking,
        expires_at: std::time::Instant::now() + THINKING_CACHE_TTL,
    };
    cache
        .write()
        .unwrap_or_else(|e| {
            tracing::error!("thinking cache RwLock poisoned; recovered");
            e.into_inner()
        })
        .put(key, entry);
}

/// Look up a cached thinking observation by `(provider_id, tool_use_id)`.
/// Returns `None` if the key is absent or the entry has expired.
///
/// Peek (rather than `get`) so a stale-but-not-yet-expired entry is not
/// promoted to MRU and held past its natural eviction. Reasoning blocks
/// may carry sensitive context; we want them to die on schedule, not be
/// revived by reads.
pub(crate) fn lookup_thinking(
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    tool_use_id: &str,
) -> Option<Vec<routectl_core::ReasoningDetail>> {
    let key = (provider_id.to_string(), tool_use_id.to_string());
    let guard = cache.read().unwrap_or_else(|e| {
        tracing::error!("thinking cache RwLock poisoned; recovered");
        e.into_inner()
    });
    guard.peek(&key).and_then(|entry| {
        if std::time::Instant::now() < entry.expires_at {
            Some(entry.thinking.clone())
        } else {
            None
        }
    })
}

/// Build a `Text`-kind `ReasoningDetail` carrying an Anthropic Thinking
/// block's `(text, signature)` pair. Shared by the non-streaming
/// extraction path (`extract_tool_thinking`) and the streaming
/// aggregation terminal in `sse.rs` so both produce byte-identical
/// detail shapes for replay.
pub(crate) fn make_thinking_detail(
    id: String,
    index: u32,
    text: String,
    signature: String,
) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some(id),
        format: Some(super::ANTHROPIC_FORMAT.to_string()),
        index: Some(index),
        payload: serde_json::json!({"text": text, "signature": signature}),
    }
}

/// Build an `Encrypted`-kind `ReasoningDetail` carrying an Anthropic
/// RedactedThinking block's opaque `data` field. Shared by the
/// non-streaming extraction path (`extract_tool_thinking`) and the
/// streaming `redacted_thinking` block-start branch in `sse.rs` so both
/// produce byte-identical detail shapes for replay.
pub(crate) fn make_redacted_thinking_detail(
    id: String,
    index: u32,
    data: String,
) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: Some(id),
        format: Some(super::ANTHROPIC_FORMAT.to_string()),
        index: Some(index),
        payload: serde_json::json!({"data": data}),
    }
}

/// Walk a flat content-block slice and return `(tool_use_id, thinking)`
/// pairs for cache storage. Rules:
/// - Thinking / RedactedThinking blocks accumulate into a running vec.
/// - ToolUse blocks with a non-empty id emit one pair: the id and a
///   CLONE of the running vec at that point. The running vec IS cleared
///   after emission so each tool_use carries only the thinking that
///   immediately preceded it (non-cumulative).
/// - All other variants (Text, Image, Document, Other, ...) are skipped.
/// - A ToolUse with an empty id is silently skipped.
///
/// Returns an empty vec when no qualifying ToolUse blocks are present.
pub(crate) fn extract_tool_thinking(
    blocks: &[crate::anthropic_api::types::ContentBlock],
) -> Vec<(String, Vec<routectl_core::ReasoningDetail>)> {
    use crate::anthropic_api::types::ContentBlock;
    use uuid::Uuid;

    let mut running: Vec<ReasoningDetail> = Vec::new();
    let mut result: Vec<(String, Vec<ReasoningDetail>)> = Vec::new();
    let mut detail_index: u32 = 0;

    for block in blocks {
        match block {
            ContentBlock::Thinking {
                thinking,
                signature,
                ..
            } => {
                running.push(make_thinking_detail(
                    Uuid::new_v4().to_string(),
                    detail_index,
                    thinking.clone(),
                    signature.clone(),
                ));
                detail_index += 1;
            }
            ContentBlock::RedactedThinking { data, .. } => {
                running.push(make_redacted_thinking_detail(
                    Uuid::new_v4().to_string(),
                    detail_index,
                    data.clone(),
                ));
                detail_index += 1;
            }
            ContentBlock::ToolUse { id, .. } if !id.is_empty() => {
                result.push((id.clone(), running.clone()));
                // Non-cumulative: each tool_use is only paired with the
                // thinking that immediately preceded it. Clear the running
                // vec so the next tool_use in the same response starts
                // fresh.
                running.clear();
            }
            _ => {}
        }
    }
    result
}

// ---------------------------------------------------------------------------
// apply_clear_thinking_edit
// ---------------------------------------------------------------------------

/// Result of calling `apply_clear_thinking_edit`.
pub(crate) struct ApplyResult {
    /// Tool-use ids whose thinking could not be found in the cache
    /// (cold-start or TTL eviction). Callers should soft-fail by
    /// stripping the `thinking` body key to avoid upstream 400s.
    pub(crate) missed_tool_ids: Vec<String>,
}

/// Internal keep-policy parsed from the `clear_thinking_20251015` edit.
enum KeepPolicy {
    All,
    LastN(usize),
    None,
}

/// Convert a `ReasoningDetail` to the corresponding Anthropic wire block.
///
/// Returns `None` when the detail should be skipped rather than emitted:
/// - `Summary` kind: not an Anthropic block type.
/// - `Text` kind with empty or missing format (not `anthropic-claude-v1`):
///   would produce a Thinking block Anthropic rejects.
/// - `Text` kind with empty signature: Anthropic 400s on unsigned Thinking
///   blocks; omitting is better than a hard fail.
/// - `Encrypted` kind with wrong format: same safety posture.
///
/// Guards mirror `emit_reasoning_blocks` in `request.rs` so both the
/// on-request replay path and the context-management inject path apply
/// the same validation. Keep them in sync when either function changes.
fn reasoning_detail_to_thinking_block(
    rd: &routectl_core::ReasoningDetail,
) -> std::option::Option<crate::anthropic_api::types::ContentBlock> {
    use crate::anthropic_api::types::ContentBlock;
    use routectl_core::ReasoningDetailKind;
    match rd.kind {
        ReasoningDetailKind::Text => {
            if rd.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                return None;
            }
            let signature = rd.payload["signature"].as_str().unwrap_or("");
            if signature.is_empty() {
                return None;
            }
            Some(ContentBlock::Thinking {
                thinking: rd.payload["text"].as_str().unwrap_or("").to_string(),
                signature: signature.to_string(),
                cache_control: std::option::Option::None,
            })
        }
        ReasoningDetailKind::Encrypted => {
            if rd.format.as_deref() != Some(super::ANTHROPIC_FORMAT) {
                return None;
            }
            Some(ContentBlock::RedactedThinking {
                data: rd.payload["data"].as_str().unwrap_or("").to_string(),
                cache_control: std::option::Option::None,
            })
        }
        // Summary is an OpenAI Responses construct; not a valid
        // Anthropic block type. Silently skip it.
        ReasoningDetailKind::Summary => None,
    }
}

/// Apply the `clear_thinking_20251015` edit from the inbound
/// `provider_extras["context_management"]["edits"]` array against an
/// already-translated `Vec<AnthropicMessage>`. Injects cached thinking
/// blocks before each qualifying ToolUse block per the `keep` policy.
///
/// Ordering invariant: call AFTER `translate_messages()` returns but
/// BEFORE the body is serialized. Injections are in-place via index
/// arithmetic (forward pass, offset-tracked) so each shift is O(n) in
/// the content-block slice -- acceptable for typical assistant turns.
///
/// Returns an `ApplyResult` the caller uses to decide whether to
/// soft-fail by stripping the `thinking` body key.
pub(crate) fn apply_clear_thinking_edit(
    messages: &mut [crate::anthropic_api::types::AnthropicMessage],
    extras: std::option::Option<&serde_json::Value>,
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
) -> ApplyResult {
    use crate::anthropic_api::types::AnthropicContent;

    let Some(edit) = find_clear_thinking_edit(extras) else {
        return ApplyResult {
            missed_tool_ids: vec![],
        };
    };

    let policy = parse_keep_policy(edit, provider_id);
    let selected = select_target_message_indices(messages, &policy);
    if selected.is_empty() {
        return ApplyResult {
            missed_tool_ids: vec![],
        };
    }

    let mut missed_tool_ids: Vec<String> = Vec::new();
    for msg_idx in &selected {
        let msg = &mut messages[*msg_idx];
        let blocks = match &mut msg.content {
            AnthropicContent::Blocks(b) => b,
            AnthropicContent::Text(_) => continue,
        };
        inject_thinking_into_message(blocks, cache, provider_id, &mut missed_tool_ids);
    }

    ApplyResult { missed_tool_ids }
}

/// Locate the first `clear_thinking_20251015` entry in
/// `extras["context_management"]["edits"]`. Returns `None` when extras
/// are absent, malformed, or contain no matching edit.
fn find_clear_thinking_edit(
    extras: std::option::Option<&serde_json::Value>,
) -> std::option::Option<&serde_json::Value> {
    extras
        .and_then(|e| e.get("context_management"))
        .and_then(|cm| cm.get("edits"))
        .and_then(|edits| edits.as_array())
        .and_then(|arr| {
            arr.iter().find(|e| {
                e.get("type").and_then(serde_json::Value::as_str)
                    == std::option::Option::Some(CLEAR_THINKING_EDIT_TYPE)
            })
        })
}

/// Decode the `keep` field on a `clear_thinking_20251015` edit. Accepts:
/// - `"all"` -> `KeepPolicy::All`
/// - bare integer 0 / N -> `None` / `LastN(N)`
/// - `{"type":"thinking_turns","value":n}` -> `None` / `LastN(N)`
/// - missing or unknown shape -> defaults to `KeepPolicy::All` and emits
///   a debug log so operators can spot malformed inputs.
fn parse_keep_policy(edit: &serde_json::Value, provider_id: &str) -> KeepPolicy {
    let keep_val = edit.get("keep");
    match keep_val {
        std::option::Option::Some(serde_json::Value::String(s)) if s == "all" => KeepPolicy::All,
        // Bare integer: keep = 0 means None, keep = N means LastN(N).
        std::option::Option::Some(serde_json::Value::Number(n)) => match n.as_u64() {
            std::option::Option::Some(0) => KeepPolicy::None,
            std::option::Option::Some(n) => KeepPolicy::LastN(n as usize),
            std::option::Option::None => {
                tracing::debug!(
                    provider = %provider_id,
                    "non-integer bare keep value in clear_thinking edit; defaulting to all"
                );
                KeepPolicy::All
            }
        },
        std::option::Option::Some(v) => {
            let typ = v.get("type").and_then(serde_json::Value::as_str);
            let n = v.get("value").and_then(serde_json::Value::as_u64);
            match (typ, n) {
                (std::option::Option::Some("thinking_turns"), std::option::Option::Some(0)) => {
                    KeepPolicy::None
                }
                (std::option::Option::Some("thinking_turns"), std::option::Option::Some(n)) => {
                    KeepPolicy::LastN(n as usize)
                }
                _ => {
                    tracing::debug!(
                        provider = %provider_id,
                        ?v,
                        "unknown keep policy in clear_thinking edit; defaulting to all"
                    );
                    KeepPolicy::All
                }
            }
        }
        std::option::Option::None => {
            tracing::debug!(
                provider = %provider_id,
                "missing keep field in clear_thinking edit; defaulting to all"
            );
            KeepPolicy::All
        }
    }
}

/// Choose which assistant-message indices receive injection under the
/// given keep policy. Walks `messages` to find assistant messages that
/// contain at least one `ToolUse` block, then narrows by policy:
/// - `All` -> every qualifying index.
/// - `LastN(n)` -> the last `n` qualifying indices.
/// - `None` -> the empty set.
fn select_target_message_indices(
    messages: &[crate::anthropic_api::types::AnthropicMessage],
    policy: &KeepPolicy,
) -> std::collections::HashSet<usize> {
    use crate::anthropic_api::types::{AnthropicContent, AnthropicRole, ContentBlock};

    let qualifying: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if !matches!(m.role, AnthropicRole::Assistant) {
                return std::option::Option::None;
            }
            let has_tool_use = match &m.content {
                AnthropicContent::Blocks(blocks) => blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
                AnthropicContent::Text(_) => false,
            };
            if has_tool_use {
                std::option::Option::Some(i)
            } else {
                std::option::Option::None
            }
        })
        .collect();

    match policy {
        KeepPolicy::All => qualifying.iter().copied().collect(),
        KeepPolicy::LastN(n) => {
            let start = qualifying.len().saturating_sub(*n);
            qualifying[start..].iter().copied().collect()
        }
        KeepPolicy::None => std::collections::HashSet::new(),
    }
}

/// Inject cached thinking before each `ToolUse` block in `blocks`.
///
/// Forward pass with offset tracking so each shift is O(n) in the
/// content-block slice. Per tool_use:
/// - Idempotency guard: if the immediately preceding block is already
///   `Thinking` or `RedactedThinking`, skip without consulting the cache.
/// - Otherwise delegate the lookup-and-insert to
///   `try_inject_thinking_at` and bump the running offset by the number
///   of blocks it inserted.
fn inject_thinking_into_message(
    blocks: &mut Vec<crate::anthropic_api::types::ContentBlock>,
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    missed_tool_ids: &mut Vec<String>,
) {
    use crate::anthropic_api::types::ContentBlock;

    let tool_use_pairs: Vec<(usize, String)> = blocks
        .iter()
        .enumerate()
        .filter_map(|(j, b)| {
            if let ContentBlock::ToolUse { id, .. } = b {
                std::option::Option::Some((j, id.clone()))
            } else {
                std::option::Option::None
            }
        })
        .collect();

    let mut offset: usize = 0;
    for (orig_j, id) in tool_use_pairs {
        let current_j = orig_j + offset;
        if current_j > 0
            && matches!(
                &blocks[current_j - 1],
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. }
            )
        {
            continue;
        }
        offset +=
            try_inject_thinking_at(blocks, current_j, id, cache, provider_id, missed_tool_ids);
    }
}

/// Look up cached thinking for `tool_use_id` and, on a usable hit,
/// insert the resulting blocks at `current_j`. Returns the number of
/// blocks inserted so the caller can advance its offset cursor.
///
/// - Cache hit with non-empty details: filter through
///   `reasoning_detail_to_thinking_block`. If every detail is filtered
///   (wrong format, empty signature, or Summary kind), record a miss so
///   the caller can soft-fail; otherwise insert the survivors and
///   return their count.
/// - Cache hit with empty `Vec`: success with nothing to inject; not a
///   miss; return 0.
/// - Cache miss (cold-start or TTL eviction): record the tool_use id so
///   the caller can strip the `thinking` body key; return 0.
fn try_inject_thinking_at(
    blocks: &mut Vec<crate::anthropic_api::types::ContentBlock>,
    current_j: usize,
    tool_use_id: String,
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    missed_tool_ids: &mut Vec<String>,
) -> usize {
    use crate::anthropic_api::types::ContentBlock;

    match lookup_thinking(cache, provider_id, &tool_use_id) {
        std::option::Option::Some(details) => {
            if details.is_empty() {
                // Some([]) -- upstream produced this tool_use with no
                // preceding thinking. Success with nothing to inject; not
                // a miss.
                return 0;
            }
            let new_blocks: Vec<ContentBlock> = details
                .iter()
                .filter_map(reasoning_detail_to_thinking_block)
                .collect();
            if new_blocks.is_empty() {
                // All details were filtered (wrong format, empty
                // signature, or Summary kind). Treat as a miss so the
                // caller can soft-fail rather than silently injecting
                // nothing.
                missed_tool_ids.push(tool_use_id);
                return 0;
            }
            let insert_count = new_blocks.len();
            for (k, block) in new_blocks.into_iter().enumerate() {
                blocks.insert(current_j + k, block);
            }
            insert_count
        }
        std::option::Option::None => {
            // Real cache miss (cold-start or TTL eviction).
            missed_tool_ids.push(tool_use_id);
            0
        }
    }
}

#[cfg(test)]
mod tests {
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
            MAX_THINKING_ENTRY_BYTES,
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
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        cache.write().expect("lock").put(key, entry);
        let result = lookup_thinking(&cache, "provider-b", "tool-old");
        assert!(result.is_none(), "expected None for expired entry");
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
        snap(&cache, "prov", "tool-x", first.clone());
        snap(&cache, "prov", "tool-x", second.clone());
        let result = lookup_thinking(&cache, "prov", "tool-x")
            .expect("entry should exist after second insert");
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
            format: format.map(|s| s.to_string()),
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
        assert!(
            matches!(&blocks[0], ContentBlock::Thinking { thinking, .. } if thinking == "think-a")
        );
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
        // ~100 KB payload, well under the 256 KB default cap.
        let thinking = make_thinking_of_size(100 * 1024);
        snapshot_to_cache(
            &cache,
            "prov",
            "tool-under",
            thinking.clone(),
            MAX_THINKING_ENTRY_BYTES,
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
        // ~300 KB payload, well over the 256 KB default cap.
        let thinking = make_thinking_of_size(300 * 1024);
        snapshot_to_cache(
            &cache,
            "prov",
            "tool-over",
            thinking,
            MAX_THINKING_ENTRY_BYTES,
            "stream",
        );
        assert!(
            lookup_thinking(&cache, "prov", "tool-over").is_none(),
            "over-cap entry must be rejected; lookup should miss"
        );
    }

    /// A configurable cap (passed by callers from `[providers.X]
    /// max_thinking_entry_bytes`) must be honored: a 1024-byte cap rejects
    /// a 2 KB entry that would otherwise pass under the 256 KB default.
    #[test]
    fn snapshot_to_cache_honors_per_call_cap_override() {
        let cache = small_cache(4);
        let thinking = make_thinking_of_size(2 * 1024);
        // 2 KB entry is well under the default 256 KB cap...
        snapshot_to_cache(
            &cache,
            "prov",
            "tool-default",
            thinking.clone(),
            MAX_THINKING_ENTRY_BYTES,
            "complete",
        );
        assert!(
            lookup_thinking(&cache, "prov", "tool-default").is_some(),
            "2 KB entry must round-trip under the default cap"
        );
        // ...but rejected under a tightened 1024-byte cap.
        snapshot_to_cache(&cache, "prov", "tool-tight", thinking, 1024, "complete");
        assert!(
            lookup_thinking(&cache, "prov", "tool-tight").is_none(),
            "2 KB entry must be rejected when the per-call cap is 1 KB"
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
}
