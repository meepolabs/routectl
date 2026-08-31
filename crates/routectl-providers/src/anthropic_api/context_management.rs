//! Server-side emulation of Anthropic's context-management-2025-06-27 beta
//! for non-Anthropic anthropic-api providers. Stores thinking blocks observed
//! in upstream responses for re-injection on next-turn requests where
//! claude-code stripped them under the beta. See docs/PROVIDER-QUIRKS.md for
//! the operator-level explanation.

use routectl_core::{ReasoningDetail, ReasoningDetailKind};

/// Beta flag that enables Anthropic's server-side context-management.
/// Stripped from outgoing headers when emulation mode is active.
pub const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Edit type tag for the thinking-strip edit in context_management edits arrays.
pub const CLEAR_THINKING_EDIT_TYPE: &str = "clear_thinking_20251015";

/// Cache key: `(provider_id, tool_use_id)`.
/// The provider_id scope ensures that two providers sharing the same
/// tool_use_id (unlikely but possible under multi-provider configs) never
/// cross-contaminate each other's thinking stores.
pub type ThinkingCacheKey = (String, String);

/// A single cached thinking observation.
pub struct ThinkingCacheEntry {
    /// The reasoning blocks captured from the upstream response that
    /// followed the tool_use block identified by the cache key.
    pub(crate) thinking: Vec<ReasoningDetail>,
    /// Wall-clock expiry. The store evicts entries that are past this
    /// instant on the next access attempt (checked by the reader).
    /// Refreshed to `Instant::now() + ttl` on every successful hit
    /// (sliding TTL).
    pub(crate) expires_at: std::time::Instant,
    /// TTL applied at write time and reused on every hit to refresh
    /// `expires_at`. Stored so a per-call TTL override (test or future
    /// per-provider knob) keeps applying through the entry's lifetime
    /// rather than silently snapping to the hardcoded constant on the
    /// first hit.
    pub(crate) ttl: std::time::Duration,
}

/// LRU map from `(provider_id, tool_use_id)` to a thinking observation.
/// Bounded at `THINKING_CACHE_CAP` (10000); oldest entries are evicted
/// when the cap is reached (standard LRU semantics).
pub type ThinkingCache = lru::LruCache<ThinkingCacheKey, ThinkingCacheEntry>;

/// Maximum number of `(provider_id, tool_use_id)` entries the
/// thinking-cache LRU will hold before evicting the oldest entry on
/// the next write. `THINKING_CACHE_CAP * DEFAULT_MAX_THINKING_ENTRY_BYTES`
/// is the LRU's worst-case memory footprint (`10_000 * 1 MiB ~ 10 GiB`).
/// Operators sizing memory on memory-constrained hosts should tune the
/// per-provider `max_thinking_entry_bytes` knob down.
pub const THINKING_CACHE_CAP: usize = 10_000;

/// TTL on entries in the thinking cache used by the `context_management`
/// emulation path. Entries older than this duration are treated as
/// stale and discarded on the next read. 60 minutes matches the typical
/// maximum agentic session length before context rotation.
pub const THINKING_CACHE_TTL: std::time::Duration = std::time::Duration::from_hours(1);

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
/// Default for the per-provider `max_thinking_entry_bytes` knob. 1 MiB
/// gives ~3x headroom over the realistic worst case for Opus 4.6/4.7/4.8
/// reasoning turns at full 65k thinking-token budgets (~328 KB at
/// ~5 bytes/token). Operators on memory-constrained hosts can tune
/// down via `[providers.X].max_thinking_entry_bytes`. The LRU's
/// worst-case footprint is `THINKING_CACHE_CAP * cap` (10_000 * 1 MiB
/// ~ 10 GiB at the default).
pub const DEFAULT_MAX_THINKING_ENTRY_BYTES: usize = 1024 * 1024;

/// Store a thinking observation into the cache under `(provider_id, tool_use_id)`.
/// Overwrites any existing entry for the same key.
///
/// Rejects writes whose serialized JSON byte length exceeds
/// `max_entry_bytes`. The serialization round-trip is cheap relative
/// to the cache write itself and captures every payload field
/// (text + signature + data). On rejection the LRU is NOT touched and
/// a structured WARN is emitted so operators can grep for oversized
/// inputs. `path` tags the call site ("complete" / "stream") in the
/// log. `ttl` is the operator-configured TTL applied to this entry's
/// `expires_at`.
pub fn snapshot_to_cache(
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    tool_use_id: &str,
    thinking: Vec<routectl_core::ReasoningDetail>,
    max_entry_bytes: usize,
    ttl: std::time::Duration,
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
    let observed_bytes = serde_json::to_vec(&thinking).map_or(0, |v| v.len());
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
        expires_at: std::time::Instant::now() + ttl,
        ttl,
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
/// Sliding TTL: every hit refreshes the entry's `expires_at` to
/// `ttl-from-now`, matching Anthropic and DeepSeek prompt-cache
/// semantics. Idle entries die after the configured TTL window.
/// `get` (rather than `peek`) also promotes the hit to MRU so cache
/// pressure preferentially evicts unused entries first.
///
/// NOTE: takes a write lock rather than read because both LRU
/// promotion (`get_mut`) and the `expires_at` refresh require
/// mutable access. Acceptable under routectl's single-process
/// local-machine target; revisit with `parking_lot::RwLock`
/// upgradable-read or sharded storage if concurrent read pressure
/// ever grows.
pub fn lookup_thinking(
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    tool_use_id: &str,
) -> Option<Vec<routectl_core::ReasoningDetail>> {
    let key = (provider_id.to_string(), tool_use_id.to_string());
    let mut guard = cache.write().unwrap_or_else(|e| {
        tracing::error!("thinking cache RwLock poisoned; recovered");
        e.into_inner()
    });
    let entry = guard.get_mut(&key)?;
    let now = std::time::Instant::now();
    if now >= entry.expires_at {
        // Expired: evict the dead entry rather than leaving it occupying a
        // slot. `get_mut` already promoted it to MRU, so without this pop
        // the stale entry would sit at the front of the eviction queue and
        // crowd out live entries under cache pressure.
        guard.pop(&key);
        return None;
    }
    entry.expires_at = now + entry.ttl;
    Some(entry.thinking.clone())
}

/// Build a `Text`-kind `ReasoningDetail` carrying an Anthropic Thinking
/// block's `(text, signature)` pair. Shared by the non-streaming
/// extraction path (`extract_tool_thinking`) and the streaming
/// aggregation terminal in `sse.rs` so both produce byte-identical
/// detail shapes for replay.
pub(super) fn make_thinking_detail(
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
pub(super) fn make_redacted_thinking_detail(
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
pub fn extract_tool_thinking(
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
pub struct ApplyResult {
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
/// Guards mirror `emit_reasoning_blocks` in `messages.rs` so both the
/// on-request replay path and the context-management inject path apply
/// the same validation. Keep them in sync when either function changes.
///
/// `envelopes` carries the request's reasoning-envelope policy, shared
/// with the message-translation channel: a cached `Encrypted` detail's
/// `data` is client-origin bytes (the cache stores what a client-supplied
/// or upstream-observed detail carried), so a wrapped envelope can reach
/// the wire through this path too, and both paths must feed one tally so
/// the aggregated WARN stays at one line per request.
fn reasoning_detail_to_thinking_block(
    rd: &routectl_core::ReasoningDetail,
    envelopes: &mut crate::anthropic_api::envelope_policy::EnvelopeUnwrapTally,
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
                data: envelopes.wire_data(rd.payload["data"].as_str().unwrap_or("")),
                cache_control: std::option::Option::None,
            })
        }
        // Summary is an OpenAI Responses construct; not a valid
        // Anthropic block type. Silently skip it.
        //
        // An unrecognized kind is a cross-dialect translation drop: it has no
        // Anthropic block shape to translate into either, so it gets the
        // same treatment as Summary rather than a new one.
        ReasoningDetailKind::Summary | ReasoningDetailKind::Other(_) => None,
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
pub fn apply_clear_thinking_edit(
    messages: &mut [crate::anthropic_api::types::AnthropicMessage],
    extras: std::option::Option<&serde_json::Value>,
    cache: &std::sync::RwLock<ThinkingCache>,
    provider_id: &str,
    envelopes: &mut crate::anthropic_api::envelope_policy::EnvelopeUnwrapTally,
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
        inject_thinking_into_message(blocks, cache, provider_id, &mut missed_tool_ids, envelopes);
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
            std::option::Option::Some(n) => {
                KeepPolicy::LastN(usize::try_from(n).unwrap_or(usize::MAX))
            }
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
                    KeepPolicy::LastN(usize::try_from(n).unwrap_or(usize::MAX))
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
    envelopes: &mut crate::anthropic_api::envelope_policy::EnvelopeUnwrapTally,
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
        offset += try_inject_thinking_at(
            blocks,
            current_j,
            id,
            cache,
            provider_id,
            missed_tool_ids,
            envelopes,
        );
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
    envelopes: &mut crate::anthropic_api::envelope_policy::EnvelopeUnwrapTally,
) -> usize {
    use crate::anthropic_api::types::ContentBlock;

    if let std::option::Option::Some(details) = lookup_thinking(cache, provider_id, &tool_use_id) {
        if details.is_empty() {
            // Some([]) -- upstream produced this tool_use with no
            // preceding thinking. Success with nothing to inject; not
            // a miss.
            return 0;
        }
        let new_blocks: Vec<ContentBlock> = details
            .iter()
            .filter_map(|rd| reasoning_detail_to_thinking_block(rd, envelopes))
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
    } else {
        // Real cache miss (cold-start or TTL eviction).
        missed_tool_ids.push(tool_use_id);
        0
    }
}

#[cfg(test)]
#[path = "context_management_tests.rs"]
mod tests;
