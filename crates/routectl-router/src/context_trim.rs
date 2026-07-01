//! Pure, deterministic STEADY-STATE context trimmer (advisory MVP).
//!
//! This module proposes a cache-coherent prefix trim for a long conversation
//! and produces a [`PrefixReductionCandidate`] the existing
//! [`crate::cost_gate::evaluate`] can price. It is ADVISORY-ONLY: nothing here
//! mutates a live request or touches the dispatch path. [`apply_trim_plan`]
//! returns a CLONE with the trim applied -- used by tests now and a later live
//! increment.
//!
//! THE CUT RULE (deterministic, FRONT-ANCHORED, quality-guarded):
//! the trimmer elides bulky OLD tool content by PLACEHOLDER SUBSTITUTION (the
//! Anthropic `clear_tool_uses` model), never whole-message removal. Keeping the
//! message sequence intact means a `tool_use` can never be orphaned from its
//! `tool_result`. The elided span is selected as a pure function of FRONT
//! content only: start after a fixed head, scan FORWARD marking the oldest
//! elidable tool content until enough tokens are freed, and never enter the
//! protected recent tail. Because a growing conversation APPENDS at the tail,
//! front message indices are immutable turn-to-turn, so the same span is elided
//! every turn and the upstream exact-prefix cache stays warm on the trimmed
//! prefix.
//!
//! A back-anchored ("last N messages") boundary is the cardinal anti-pattern:
//! it shifts every turn and destroys cache coherence. `keep_recent_messages`
//! only sets a FLOOR the forward scan must not cross; it never makes the elided
//! span itself depend on total conversation length.
//!
//! Every doubt resolves to None (no plan): below the trigger, too short to
//! carry a safe head plus a `clear_at_least`-sized elidable old-tool span, or
//! any structural uncertainty.

use std::ops::Range;

use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::schema::{ChatRequest, MessageContent};
use serde_json::Value;

use crate::cost_gate::PrefixReductionCandidate;

/// FNV-1a 64-bit offset basis and prime. Inline implementation avoids a
/// new crate dep; FNV is stable across Rust toolchain versions (unlike
/// `std::collections::hash_map::DefaultHasher`, whose output is explicitly
/// not guaranteed stable across versions or compilations).
const FNV_OFFSET_BASIS: u64 = 14695981039346656037;
const FNV_PRIME: u64 = 1099511628211;

/// Compute an FNV-1a 64-bit hash of an arbitrary byte slice.
fn fnv1a_hash(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Bytes-per-token divisor for the rough token estimate. Matches
/// `routectl_core::context_reduction`'s `BYTES_PER_TOKEN_ESTIMATE` so the
/// trimmer's `d` / `c` / `c_after` counts are consistent with the rest of the
/// advisory (an operator-facing signal, not a billing figure).
const BYTES_PER_TOKEN_ESTIMATE: u64 = 4;

/// Fixed, content-independent placeholder substituted for an elided tool
/// payload. Content-independent so the plan is deterministic and byte-stable
/// across turns; phrased so the model knows content was elided.
const ELISION_PLACEHOLDER: &str = "[elided: prior tool content removed to bound context]";

/// Conservative default trigger: start trimming only once the conversation is
/// large enough that a bounded cut is worthwhile.
const DEFAULT_TRIGGER_TOKENS: u64 = 100_000;
/// Conservative default minimum freed tokens per cut.
const DEFAULT_CLEAR_AT_LEAST_TOKENS: u64 = 20_000;
/// Conservative default head messages to keep intact (system framing / first
/// turns sit at the very front and are the most cache-valuable to preserve).
const DEFAULT_HEAD_KEEP_MESSAGES: usize = 2;
/// Conservative default recent messages to protect from elision.
const DEFAULT_KEEP_RECENT_MESSAGES: usize = 6;

/// Knobs for the steady-state trim. Operator-config wiring is deferred; the
/// [`Default`] carries the conservative named consts above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SteadyStateTrimParams {
    /// Estimated total tokens at or below which no trim is proposed.
    pub trigger_tokens: u64,
    /// Minimum tokens the elided span must free for a trim to be proposed.
    pub clear_at_least_tokens: u64,
    /// Number of leading messages kept fully intact (never elided).
    pub head_keep_messages: usize,
    /// Number of trailing messages protected from elision.
    pub keep_recent_messages: usize,
}

impl Default for SteadyStateTrimParams {
    fn default() -> Self {
        Self {
            trigger_tokens: DEFAULT_TRIGGER_TOKENS,
            clear_at_least_tokens: DEFAULT_CLEAR_AT_LEAST_TOKENS,
            head_keep_messages: DEFAULT_HEAD_KEEP_MESSAGES,
            keep_recent_messages: DEFAULT_KEEP_RECENT_MESSAGES,
        }
    }
}

/// A single elision mark: which message, which content-part index, and the
/// original payload-token count freed by replacing it with the placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElisionMark {
    /// Index into `req.messages`.
    pub message_index: usize,
    /// Index into that message's `MessageContent::Parts`.
    pub part_index: usize,
    /// Estimated tokens of the ORIGINAL payload (before placeholder).
    pub original_tokens: u64,
}

/// A proposed steady-state trim. Immutable; [`apply_trim_plan`] clones and
/// applies it without mutating the source request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteadyStateTrimPlan {
    /// The economics candidate (`d` / `c_after` / `c`) for the cost gate.
    pub candidate: PrefixReductionCandidate,
    /// The half-open message-index span `[start, end)` whose old tool content
    /// is elided. `start == head_keep_messages`; `end` is one past the last
    /// elided message.
    pub span: Range<usize>,
    /// The per-part elision marks, in forward order.
    pub marks: Vec<ElisionMark>,
}

/// Propose a deterministic steady-state trim, or `None` when no safe,
/// trigger-clearing cut exists.
///
/// Pure function of `req` content + `params`: no randomness, no clock, no
/// external state. Same content -> identical plan.
#[must_use]
pub fn propose_steady_state_trim(
    req: &ChatRequest,
    params: &SteadyStateTrimParams,
) -> Option<SteadyStateTrimPlan> {
    let c = estimate_total_tokens(req);
    if c <= params.trigger_tokens {
        return None;
    }

    let n = req.messages.len();
    // The forward scan runs over [head_keep, scan_end): after the fixed head,
    // never into the protected recent tail. A floor that meets or crosses the
    // head leaves no elidable span.
    let scan_end = n.checked_sub(params.keep_recent_messages)?;
    if scan_end <= params.head_keep_messages {
        return None;
    }

    // `collect_elision_marks` is the SINGLE source of truth for the
    // `clear_at_least` floor: a `Some` here already freed at least that many
    // tokens, so no separate re-check is needed.
    let marks = collect_elision_marks(
        req,
        params.head_keep_messages,
        scan_end,
        params.clear_at_least_tokens,
    )?;

    let elided_tokens: u64 = marks.iter().map(|m| m.original_tokens).sum();

    let placeholder_tokens = estimate_str_tokens(ELISION_PLACEHOLDER);
    // `d` = original elided payload tokens minus the placeholder tokens that
    // replace each one. Saturating so a (pathological) larger-placeholder case
    // never underflows; such a case frees < clear_at_least and was already
    // rejected above for real inputs.
    let replaced = placeholder_tokens.saturating_mul(marks.len() as u64);
    let d = elided_tokens.saturating_sub(replaced);

    let span_start = params.head_keep_messages;
    let span_end = marks.last().map(|m| m.message_index + 1)?;

    // `c_after` = cached tokens from the FIRST elided position to the end of the
    // cached prefix (the one-time re-write suffix). The trimmer treats the whole
    // request as the cacheable prefix (no separate frozen-prefix slice offline),
    // so c_after is the token footprint of messages[span_start..].
    let c_after = estimate_messages_tokens(req, span_start..n);

    let candidate = PrefixReductionCandidate::new(d, c_after, c);
    Some(SteadyStateTrimPlan {
        candidate,
        span: span_start..span_end,
        marks,
    })
}

/// Scan FORWARD over `[start, scan_end)`, marking the oldest elidable tool
/// content until `clear_at_least` tokens are freed. SINGLE SOURCE OF TRUTH
/// for the `clear_at_least` floor: returns `None` whenever the scan window
/// cannot reach it (no elidable old-tool content at all, or not enough of
/// it), so the caller never sees a partial mark set it has to re-reject.
fn collect_elision_marks(
    req: &ChatRequest,
    start: usize,
    scan_end: usize,
    clear_at_least: u64,
) -> Option<Vec<ElisionMark>> {
    let mut marks: Vec<ElisionMark> = Vec::new();
    let mut freed: u64 = 0;

    for message_index in start..scan_end {
        let MessageContent::Parts(parts) = &req.messages[message_index].content else {
            continue;
        };
        for (part_index, part) in parts.iter().enumerate() {
            let Some(tokens) = elidable_part_tokens(part) else {
                continue;
            };
            marks.push(ElisionMark {
                message_index,
                part_index,
                original_tokens: tokens,
            });
            freed = freed.saturating_add(tokens);
            if freed >= clear_at_least {
                return Some(marks);
            }
        }
    }

    // The whole scan window was exhausted without freeing `clear_at_least`
    // tokens: there is no trigger-clearing cut, so report no plan.
    None
}

/// Token count of an elidable tool payload, or `None` when the part is not
/// elidable old-tool content. Only `ToolResult.content` and `ToolUse.input`
/// are elidable (bulky, session-specific, no cross-turn reuse value). A part
/// already equal to the placeholder is not re-elided (idempotence guard).
fn elidable_part_tokens(part: &ContentPart) -> Option<u64> {
    let ContentPart::Known(known) = part else {
        return None;
    };
    let target = match known {
        KnownContentPart::ToolResult { content, .. } => content,
        KnownContentPart::ToolUse { input, .. } => input,
        _ => return None,
    };
    if is_placeholder(target) {
        return None;
    }
    Some(estimate_value_tokens(target))
}

/// Whether a value is already the fixed placeholder string.
fn is_placeholder(value: &Value) -> bool {
    matches!(value, Value::String(s) if s == ELISION_PLACEHOLDER)
}

/// Apply a trim plan to a CLONE of `req`, substituting each marked tool
/// payload with the fixed placeholder. Never mutates the input.
///
/// A mark whose indices no longer address an elidable part (impossible for a
/// plan freshly produced from the same `req`, but defensive against a stale
/// plan) is skipped rather than panicking.
#[must_use]
pub fn apply_trim_plan(req: &ChatRequest, plan: &SteadyStateTrimPlan) -> ChatRequest {
    let mut out = req.clone();
    for mark in &plan.marks {
        let Some(message) = out.messages.get_mut(mark.message_index) else {
            continue;
        };
        let MessageContent::Parts(parts) = &mut message.content else {
            continue;
        };
        let Some(part) = parts.get_mut(mark.part_index) else {
            continue;
        };
        substitute_placeholder(part);
    }
    out
}

/// Stable hash of the trimmed CACHEABLE FRONT for the shadow misfire monitor.
///
/// The cacheable front is `messages[0..plan.span.end]` of the request after
/// the trim plan is applied. This covers the immutable head PLUS the elided
/// span -- the only region whose byte identity must remain stable turn-to-turn
/// for the upstream exact-prefix cache to stay warm. The tail
/// (`messages[plan.span.end..]`) is intentionally EXCLUDED: it grows with
/// every appended turn and is the extension the cache indexes into, not the
/// fixed anchor. Hashing the tail would produce a different value on every
/// turn by construction, yielding a spurious Misfire on every turn.
///
/// The hash is computed over the JSON serialization of the trimmed-front
/// messages and is deterministic across calls for identical content. FNV-1a
/// 64-bit provides stable output across Rust versions (unlike
/// `DefaultHasher`). Pure: no clock, no I/O, no randomness.
#[must_use]
pub fn trimmed_prefix_fingerprint(req: &ChatRequest, plan: &SteadyStateTrimPlan) -> u64 {
    let trimmed = apply_trim_plan(req, plan);
    let front = trimmed.messages.get(..plan.span.end).unwrap_or(&[]);
    let serialized = serde_json::to_string(front).unwrap_or_default();
    fnv1a_hash(serialized.as_bytes())
}

/// Replace a tool payload with the fixed placeholder string in place. Leaves
/// non-tool parts untouched.
fn substitute_placeholder(part: &mut ContentPart) {
    let ContentPart::Known(known) = part else {
        return;
    };
    let target = match known {
        KnownContentPart::ToolResult { content, .. } => content,
        KnownContentPart::ToolUse { input, .. } => input,
        _ => return,
    };
    *target = Value::String(ELISION_PLACEHOLDER.to_string());
}

/// Rough token estimate for the whole request: serialized byte length / 4.
fn estimate_total_tokens(req: &ChatRequest) -> u64 {
    serialized_len(req) / BYTES_PER_TOKEN_ESTIMATE
}

/// Rough token estimate for a message-index range: summed serialized byte
/// length of those messages / 4.
fn estimate_messages_tokens(req: &ChatRequest, range: Range<usize>) -> u64 {
    req.messages.get(range).map_or(0, |msgs| {
        msgs.iter().map(serialized_len).sum::<u64>() / BYTES_PER_TOKEN_ESTIMATE
    })
}

/// Rough token estimate of a single JSON value's serialized length / 4.
fn estimate_value_tokens(value: &Value) -> u64 {
    serialized_len(value)
        .checked_div(BYTES_PER_TOKEN_ESTIMATE)
        .unwrap_or(0)
}

/// Rough token estimate of a string's byte length / 4.
const fn estimate_str_tokens(s: &str) -> u64 {
    (s.len() as u64) / BYTES_PER_TOKEN_ESTIMATE
}

/// Serialized JSON byte length of any serializable value. A serialize failure
/// (not expected for canonical types) contributes 0 rather than panicking.
fn serialized_len<T: serde::Serialize>(value: &T) -> u64 {
    serde_json::to_string(value)
        .map(|s| s.len() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::content_part::{ContentPart, KnownContentPart};
    use routectl_core::schema::{Message, MessageContent, Role};
    use serde_json::json;

    /// A bulky tool_result message whose content is a large JSON string.
    fn tool_result_msg(payload: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: json!(payload),
                    is_error: None,
                    cache_control: None,
                },
            )]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// An assistant tool_use message whose input is a large JSON string.
    fn tool_use_msg(payload: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_1".into(),
                name: "search".into(),
                input: json!(payload),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A bulky payload of roughly `tokens` tokens (4 bytes/token).
    fn payload_of_tokens(tokens: usize) -> String {
        "x".repeat(tokens * BYTES_PER_TOKEN_ESTIMATE as usize)
    }

    /// Build a long tool-heavy conversation: head_keep intact, then alternating
    /// bulky tool turns, then a recent tail. Each tool turn is ~`tool_tokens`.
    fn long_conversation(tool_turns: usize, tool_tokens: usize) -> ChatRequest {
        let payload = payload_of_tokens(tool_tokens);
        let mut messages = vec![
            user_msg("system framing turn one"),
            assistant_msg("acknowledged"),
        ];
        for _ in 0..tool_turns {
            messages.push(tool_use_msg(&payload));
            messages.push(tool_result_msg(&payload));
        }
        // Recent tail of small messages.
        for i in 0..6 {
            messages.push(user_msg(&format!("recent turn {i}")));
        }
        ChatRequest {
            model: "claude-opus-4-8".into(),
            messages,
            ..Default::default()
        }
    }

    // -- GUARD: trigger + too-short --------------------------------------

    #[test]
    fn below_trigger_returns_none() {
        // Arrange: a tiny conversation well below the trigger.
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            messages: vec![user_msg("hi"), assistant_msg("hello")],
            ..Default::default()
        };
        let params = SteadyStateTrimParams::default();

        // Act / Assert
        assert_eq!(propose_steady_state_trim(&req, &params), None);
    }

    #[test]
    fn too_short_to_carry_head_and_span_returns_none() {
        // Arrange: a conversation big enough to cross the trigger by one fat
        // tool turn, but with no room for a head + elidable span + tail.
        let payload = payload_of_tokens(30_000);
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            messages: vec![
                user_msg("one"),
                assistant_msg("two"),
                tool_result_msg(&payload),
            ],
            ..Default::default()
        };
        let params = SteadyStateTrimParams::default();

        // Act: the only tool turn sits inside the protected tail (keep_recent=6
        // >= 3 messages), so scan_end underflows / leaves no span.
        assert_eq!(propose_steady_state_trim(&req, &params), None);
    }

    #[test]
    fn no_elidable_tool_content_returns_none() {
        // Arrange: a big conversation made entirely of TEXT (no tool content).
        let big = payload_of_tokens(40_000);
        let mut messages = vec![user_msg("head one"), assistant_msg("head two")];
        for i in 0..4 {
            messages.push(user_msg(&format!("{big} turn {i}")));
            messages.push(assistant_msg(&format!("{big} reply {i}")));
        }
        for i in 0..6 {
            messages.push(user_msg(&format!("recent {i}")));
        }
        let req = ChatRequest {
            model: "claude-opus-4-8".into(),
            messages,
            ..Default::default()
        };
        let params = SteadyStateTrimParams::default();

        // Act / Assert: text substance is never elided.
        assert_eq!(propose_steady_state_trim(&req, &params), None);
    }

    // -- DETERMINISM ------------------------------------------------------

    #[test]
    fn same_request_twice_yields_identical_plan() {
        // Arrange
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();

        // Act
        let a = propose_steady_state_trim(&req, &params).expect("plan");
        let b = propose_steady_state_trim(&req, &params).expect("plan");

        // Assert
        assert_eq!(a, b);
    }

    // -- NO-ORPHAN / quality ----------------------------------------------

    #[test]
    fn plan_preserves_message_count_and_pairs() {
        // Arrange: a conversation with tool_use/tool_result pairs.
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();

        // Act
        let plan = propose_steady_state_trim(&req, &params).expect("plan");
        let trimmed = apply_trim_plan(&req, &plan);

        // Assert: same message count; every tool_use still has a tool_result.
        assert_eq!(trimmed.messages.len(), req.messages.len());
        for (before, after) in req.messages.iter().zip(trimmed.messages.iter()) {
            // Role + part structure is preserved; only payloads change.
            assert_eq!(
                serde_json::to_value(&before.role).unwrap(),
                serde_json::to_value(&after.role).unwrap()
            );
            assert_eq!(part_count(before), part_count(after));
            assert_eq!(part_type_tags(before), part_type_tags(after));
        }
    }

    #[test]
    fn plan_never_touches_head_or_protected_tail() {
        // Arrange
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();

        // Act
        let plan = propose_steady_state_trim(&req, &params).expect("plan");

        // Assert: every mark sits within [head_keep, len - keep_recent).
        let n = req.messages.len();
        let scan_end = n - params.keep_recent_messages;
        for mark in &plan.marks {
            assert!(
                mark.message_index >= params.head_keep_messages,
                "mark {mark:?} entered the head"
            );
            assert!(
                mark.message_index < scan_end,
                "mark {mark:?} entered the protected tail"
            );
        }
        assert_eq!(plan.span.start, params.head_keep_messages);
    }

    // -- CACHE-COHERENCE (the core invariant) -----------------------------

    #[test]
    fn growth_does_not_shift_span_or_placeholders() {
        // SCOPE: this covers the safe early-stop case (one N -> N+1 step where
        // the forward scan reaches `clear_at_least` at a fixed front index);
        // `growth_holds_span_and_marks_across_many_turns` covers the general
        // multi-turn invariant.
        // Arrange: turn N, and turn N+1 = turn N + one appended user/assistant.
        let turn_n = long_conversation(6, 12_000);
        let mut turn_n_plus_1 = turn_n.clone();
        turn_n_plus_1
            .messages
            .push(user_msg("a brand new follow-up turn"));
        turn_n_plus_1
            .messages
            .push(assistant_msg("a brand new reply"));
        let params = SteadyStateTrimParams::default();

        // Act
        let plan_n = propose_steady_state_trim(&turn_n, &params).expect("plan n");
        let plan_n1 = propose_steady_state_trim(&turn_n_plus_1, &params).expect("plan n+1");

        // Assert: the elided span + per-part marks are IDENTICAL across the
        // growth (front-anchored, byte-stable as the conversation grows).
        assert_eq!(plan_n.span, plan_n1.span, "span shifted under growth");
        assert_eq!(plan_n.marks, plan_n1.marks, "marks shifted under growth");
    }

    #[test]
    fn growth_holds_span_and_marks_across_many_turns() {
        // The general front-determined invariant: grow a long tool-heavy
        // conversation ONE turn at a time across many appended turns and prove
        // (i) once `propose_steady_state_trim` returns `Some` it NEVER reverts
        // to `None`, and (ii) across ALL `Some` turns the elided `span` AND the
        // per-part `marks` are byte-identical. Together these show the elided
        // span is fixed by FRONT content: the `None -> Some` activation is
        // one-time and there is no harmful turn-to-turn flicker.
        // Arrange: a conversation already past the trigger, then >= 10 growth
        // steps appending one user + one assistant turn each.
        const GROWTH_STEPS: usize = 12;
        let mut req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();

        // Act + Assert, step by step.
        let mut activated: Option<(Range<usize>, Vec<ElisionMark>)> = None;
        for step in 0..GROWTH_STEPS {
            match propose_steady_state_trim(&req, &params) {
                Some(plan) => match &activated {
                    None => activated = Some((plan.span.clone(), plan.marks.clone())),
                    Some((span, marks)) => {
                        assert_eq!(&plan.span, span, "span shifted at growth step {step}");
                        assert_eq!(&plan.marks, marks, "marks shifted at growth step {step}");
                    }
                },
                None => assert!(
                    activated.is_none(),
                    "plan reverted to None at growth step {step} after activating",
                ),
            }
            // Grow by one full turn (front indices stay immutable; only the
            // tail lengthens).
            req.messages
                .push(user_msg(&format!("follow-up turn {step}")));
            req.messages.push(assistant_msg(&format!("reply {step}")));
        }

        // Sanity: the trimmer DID activate at some point (otherwise the
        // invariant above is vacuously true and proves nothing).
        assert!(
            activated.is_some(),
            "trimmer never activated across the growth sweep",
        );
    }

    #[test]
    fn trimmed_prefix_is_byte_prefix_extension_under_growth() {
        // Arrange: turn N and turn N+1 (one appended turn).
        let turn_n = long_conversation(6, 12_000);
        let mut turn_n_plus_1 = turn_n.clone();
        turn_n_plus_1
            .messages
            .push(user_msg("a brand new follow-up turn"));
        turn_n_plus_1
            .messages
            .push(assistant_msg("a brand new reply"));
        let params = SteadyStateTrimParams::default();

        // Act
        let plan_n = propose_steady_state_trim(&turn_n, &params).expect("plan n");
        let plan_n1 = propose_steady_state_trim(&turn_n_plus_1, &params).expect("plan n+1");
        let trimmed_n = apply_trim_plan(&turn_n, &plan_n);
        let trimmed_n1 = apply_trim_plan(&turn_n_plus_1, &plan_n1);

        // Assert: the serialized message prefix of turn N+1 (up to N's length)
        // is byte-identical to N's -- the cut did not shift or re-grow.
        let prefix_n: Vec<_> = trimmed_n
            .messages
            .iter()
            .map(|m| serde_json::to_string(m).unwrap())
            .collect();
        let prefix_n1: Vec<_> = trimmed_n1
            .messages
            .iter()
            .take(trimmed_n.messages.len())
            .map(|m| serde_json::to_string(m).unwrap())
            .collect();
        assert_eq!(prefix_n, prefix_n1, "trimmed prefix shifted under growth");
        // Sanity: N+1 is strictly longer (the appended turn).
        assert!(trimmed_n1.messages.len() > trimmed_n.messages.len());
    }

    #[test]
    fn fingerprint_stable_across_growth_turns() {
        // The trimmed-prefix fingerprint must be byte-stable across >= 3
        // growth turns: every turn that grows the tail (appended messages)
        // must produce the SAME fingerprint as turn N, because the head
        // content the fingerprint covers is immutable.
        let base = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan_base = propose_steady_state_trim(&base, &params).expect("plan base");
        let fp_base = trimmed_prefix_fingerprint(&base, &plan_base);

        for growth in 1..=3usize {
            let mut grown = base.clone();
            for i in 0..growth {
                grown.messages.push(user_msg(&format!("growth {i}")));
                grown.messages.push(assistant_msg(&format!("reply {i}")));
            }
            let plan_g = propose_steady_state_trim(&grown, &params).expect("plan grown");
            let fp_g = trimmed_prefix_fingerprint(&grown, &plan_g);
            assert_eq!(fp_base, fp_g, "fingerprint drifted at growth step {growth}",);
        }
    }

    #[test]
    fn fingerprint_differs_when_front_content_perturbed() {
        // Perturbing the front (head) content must change the fingerprint
        // so that a real prefix shift is detected as a Misfire.
        let base = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&base, &params).expect("plan");
        let fp_base = trimmed_prefix_fingerprint(&base, &plan);

        // Replace the first message (front head) content.
        let mut perturbed = base;
        perturbed.messages[0] = user_msg("completely different first message XXXX");
        let plan_p = propose_steady_state_trim(&perturbed, &params).expect("plan perturbed");
        let fp_perturbed = trimmed_prefix_fingerprint(&perturbed, &plan_p);

        assert_ne!(
            fp_base, fp_perturbed,
            "fingerprint did not change when front content was perturbed",
        );
    }

    #[test]
    fn shadow_store_first_seen_then_stable_then_misfire() {
        // Full lifecycle: first call -> FirstSeen, same fingerprint -> Stable,
        // different fingerprint -> Misfire.
        use crate::k_estimator::{KSessionKey, ShadowOutcome, ShadowStore};
        use std::time::UNIX_EPOCH;

        let store = ShadowStore::new();
        let key = KSessionKey {
            session_key: "sess-ctx".into(),
            provider_kind: "anthropic-api".into(),
            model: "opus".into(),
        };

        let base = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&base, &params).expect("plan");
        let fp = trimmed_prefix_fingerprint(&base, &plan);

        // First call: no stored entry.
        let o1 = store.record_and_compare(key.clone(), fp, UNIX_EPOCH);
        assert_eq!(o1, ShadowOutcome::FirstSeen);

        // Same fingerprint: Stable.
        let o2 = store.record_and_compare(key.clone(), fp, UNIX_EPOCH);
        assert_eq!(o2, ShadowOutcome::Stable);

        // Different fingerprint (perturbed front): Misfire.
        let mut perturbed = base;
        perturbed.messages[0] = user_msg("different first message for misfire test");
        let plan_p = propose_steady_state_trim(&perturbed, &params).expect("plan p");
        let fp_p = trimmed_prefix_fingerprint(&perturbed, &plan_p);
        assert_ne!(fp, fp_p, "perturbed fingerprint must differ from base");
        let o3 = store.record_and_compare(key, fp_p, UNIX_EPOCH);
        assert_eq!(o3, ShadowOutcome::Misfire);
    }

    #[test]
    fn apply_trim_plan_does_not_mutate_input() {
        // Arrange
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&req, &params).expect("plan");
        let before = serde_json::to_value(&req).unwrap();

        // Act
        let _trimmed = apply_trim_plan(&req, &plan);

        // Assert: the source request is byte-identical.
        let after = serde_json::to_value(&req).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn apply_trim_plan_substitutes_fixed_placeholder() {
        // Arrange
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&req, &params).expect("plan");

        // Act
        let trimmed = apply_trim_plan(&req, &plan);

        // Assert: every marked part now carries the fixed placeholder string.
        for mark in &plan.marks {
            let parts = match &trimmed.messages[mark.message_index].content {
                MessageContent::Parts(p) => p,
                other => panic!("expected parts, got {other:?}"),
            };
            let payload = tool_payload(&parts[mark.part_index]);
            assert_eq!(payload, Some(&json!(ELISION_PLACEHOLDER)));
        }
    }

    // -- CANDIDATE MATH ---------------------------------------------------

    #[test]
    fn candidate_d_matches_elided_minus_placeholder() {
        // Arrange
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();

        // Act
        let plan = propose_steady_state_trim(&req, &params).expect("plan");

        // Assert: d == sum(original) - placeholder*count.
        let elided: u64 = plan.marks.iter().map(|m| m.original_tokens).sum();
        let placeholder = estimate_str_tokens(ELISION_PLACEHOLDER);
        let expected_d = elided - placeholder * plan.marks.len() as u64;
        assert_eq!(plan.candidate.d, expected_d);
        assert!(plan.candidate.c_after > 0);
        assert!(plan.candidate.c >= plan.candidate.c_after);
        // d freed at least clear_at_least minus the placeholder rewrites.
        assert!(elided >= params.clear_at_least_tokens);
    }

    #[test]
    fn candidate_feeds_evaluate_matching_gate_math() {
        // Arrange: a verified anthropic row + a real plan.
        use crate::cache_pricing::lookup;
        use crate::cost_gate::{GateDecision, break_even_k, evaluate};
        let req = long_conversation(8, 12_000);
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&req, &params).expect("plan");
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));

        // Act: compute K* for the candidate, then evaluate just above it.
        let k_star = break_even_k(&row, &plan.candidate).expect("d > 0");
        let decision = evaluate(&row, &plan.candidate, k_star + 1.0);

        // Assert: the verdict mirrors the gate's own computation -- a BREAK at
        // a reuse count above the candidate's own break-even threshold.
        assert_eq!(
            decision,
            GateDecision::Break {
                delta_tokens: plan.candidate.d
            }
        );
    }

    // -- helpers ----------------------------------------------------------

    fn part_count(m: &Message) -> usize {
        match &m.content {
            MessageContent::Parts(p) => p.len(),
            _ => 0,
        }
    }

    fn part_type_tags(m: &Message) -> Vec<String> {
        match &m.content {
            MessageContent::Parts(p) => p.iter().map(|x| x.type_tag().to_string()).collect(),
            _ => vec![],
        }
    }

    fn tool_payload(part: &ContentPart) -> Option<&Value> {
        match part {
            ContentPart::Known(KnownContentPart::ToolResult { content, .. }) => Some(content),
            ContentPart::Known(KnownContentPart::ToolUse { input, .. }) => Some(input),
            _ => None,
        }
    }
}
