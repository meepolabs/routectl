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

use std::collections::HashMap;
use std::ops::Range;

use routectl_core::content_part::{ContentPart, KnownContentPart};
use routectl_core::schema::{ChatRequest, MessageContent};
use serde::Serialize;
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
/// large enough that a bounded cut is worthwhile. `pub` so `config::TrimConfig`
/// can reference the SAME const its `[trim]` per-field defaults resolve to --
/// one source of truth for "missing block == these defaults".
pub const DEFAULT_TRIGGER_TOKENS: u64 = 100_000;
/// Conservative default minimum freed tokens per cut.
pub const DEFAULT_CLEAR_AT_LEAST_TOKENS: u64 = 20_000;
/// Conservative default head messages to keep intact (system framing / first
/// turns sit at the very front and are the most cache-valuable to preserve).
pub const DEFAULT_HEAD_KEEP_MESSAGES: usize = 2;
/// Conservative default recent messages to protect from elision.
pub const DEFAULT_KEEP_RECENT_MESSAGES: usize = 6;

/// Knobs for the steady-state trim. Populated from the operator-facing
/// `[trim]` block via `crate::config::TrimConfig::to_params()`; the
/// [`Default`] impl here carries the conservative named consts above and is
/// what a missing `[trim]` block resolves to. Deliberately NOT serde-derived
/// -- `TrimConfig` is the config-wire wrapper so this pure-function struct
/// stays free of a config-loading concern.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ElisionMark {
    /// Index into `req.messages`.
    pub message_index: usize,
    /// Index into that message's `MessageContent::Parts`.
    pub part_index: usize,
    /// Estimated tokens of the ORIGINAL payload (before placeholder).
    pub original_tokens: u64,
    /// Custom placeholder text for this mark, or `None` to use the fixed
    /// [`ELISION_PLACEHOLDER`]. M1 always sets `None`; a later increment
    /// (path-bearing placeholders) starts emitting `Some(...)` without a
    /// field-add or a retrofit of every mark consumer.
    pub replacement: Option<String>,
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

    // `c_after` = cached tokens from the FIRST elided message to the end of the
    // cached prefix (the one-time re-write suffix). The trimmer treats the whole
    // request as the cacheable prefix, so nothing before the first byte that
    // changes belongs in the rewrite cost -- hence the first mark, not the head
    // boundary. The `span` field below still documents the full elision span.
    let c_after = c_after_from_first_mark(req, &marks)?;

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
                replacement: None,
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
        substitute_placeholder(part, mark.replacement.as_deref());
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

/// Replace a tool payload with a placeholder string in place. Emits
/// `replacement` when `Some` (M3's path-bearing placeholders); falls back to
/// the fixed [`ELISION_PLACEHOLDER`] when `None` (M1's only case). Leaves
/// non-tool parts untouched.
fn substitute_placeholder(part: &mut ContentPart, replacement: Option<&str>) {
    let ContentPart::Known(known) = part else {
        return;
    };
    let target = match known {
        KnownContentPart::ToolResult { content, .. } => content,
        KnownContentPart::ToolUse { input, .. } => input,
        _ => return,
    };
    let text = replacement.unwrap_or(ELISION_PLACEHOLDER);
    *target = Value::String(text.to_string());
}

/// Rough token estimate for the whole request: serialized byte length / 4.
pub(crate) fn estimate_total_tokens(req: &ChatRequest) -> u64 {
    serialized_len(req) / BYTES_PER_TOKEN_ESTIMATE
}

/// Rough token estimate for a message-index range: summed serialized byte
/// length of those messages / 4.
fn estimate_messages_tokens(req: &ChatRequest, range: Range<usize>) -> u64 {
    req.messages.get(range).map_or(0, |msgs| {
        msgs.iter().map(serialized_len).sum::<u64>() / BYTES_PER_TOKEN_ESTIMATE
    })
}

/// The one-time re-write suffix cost (`c_after`): the cached-token footprint
/// from the FIRST elided message to the end of the request. The rewrite starts
/// at the first byte that changes, so the fixed head and any un-elided messages
/// before the first mark are excluded. SINGLE source of this computation for
/// both trim-candidate builders (and any later producer that feeds the same
/// cost gate). Returns `None` when there are no marks to price.
fn c_after_from_first_mark(req: &ChatRequest, marks: &[ElisionMark]) -> Option<u64> {
    let first_marked = marks.iter().map(|m| m.message_index).min()?;
    Some(estimate_messages_tokens(
        req,
        first_marked..req.messages.len(),
    ))
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
    serde_json::to_string(value).map_or(0, |s| s.len() as u64)
}

/// Conservative constant key-set used to extract a file path from a
/// `ToolUse.input` object. Only these keys are recognized; any other shape
/// fails closed (no path -> no supersession mark). Fixed order: `file_path`
/// takes precedence over `path` when both are present.
const PATH_KEYS: [&str; 2] = ["file_path", "path"];

/// Result of the near-lossless mark pass -- the measurement contract.
///
/// A pure, structured summary of the dedup + supersession heuristics over one
/// request window. The recorder maps these fields straight
/// onto `DispatchMeta`: `dedup_tokens -> would_trim_dedup_tokens`,
/// `supersession_tokens -> would_trim_supersession_tokens`,
/// `path_units -> would_trim_path_units`,
/// `path_extractable -> would_trim_path_extractable`, and `marks` feeds both
/// the raw-marks blob and the priced [`PrefixReductionCandidate`] (via
/// [`near_lossless_candidate`]).
///
/// Every field is a deterministic function of request content: the same
/// content produces an identical value (no clock, no state, no I/O).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NearLosslessMarks {
    /// Raw elision marks in transcript order (sorted by `(message_index,
    /// part_index)`), OVERLAP-DEDUPED: each marked unit appears exactly once
    /// and is attributed to exactly one heuristic. M1 marks always carry
    /// `replacement: None` (the plain placeholder).
    pub marks: Vec<ElisionMark>,
    /// Freed tokens attributed to the DEDUP heuristic (sum of the original
    /// payload tokens of every dedup-marked unit).
    pub dedup_tokens: u64,
    /// Freed tokens attributed to the SUPERSESSION heuristic (sum of the
    /// original payload tokens of every supersession-marked unit). Only stale
    /// earlier `ToolResult.content` is superseded (the paired `ToolUse` call
    /// block + its input always survive), so this counts freed RESULT-content
    /// tokens.
    pub supersession_tokens: u64,
    /// Path-attribution DENOMINATOR: the number of in-window `ToolResult`
    /// parts for which path attribution was ATTEMPTED (every result carries a
    /// `tool_use_id`). Paired with [`Self::path_extractable`] so the
    /// attribution RATE is reconstructable offline via SUM/SUM rather than
    /// pre-averaged per row.
    pub path_units: u64,
    /// Path-attribution NUMERATOR: of the [`Self::path_units`] results, how
    /// many resolved to a path -- their `tool_use_id` matched a paired
    /// `ToolUse` (id linkage) whose input yielded a [`PATH_KEYS`] entry.
    pub path_extractable: u64,
}

/// Which near-lossless heuristic marked a scanned unit. `None` means the unit
/// survives (kept). Supersession is assigned first (path-keyed); dedup runs
/// only over units still `None` (the overlap-dedup guarantee).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkKind {
    None,
    Supersession,
    Dedup,
}

/// Which elidable content part a [`ScanUnit`] came from, carrying the
/// Anthropic-shape tool linkage borrowed from the request. A `ToolUse` carries
/// its `id` (matched by a later result's `tool_use_id`) and the path extracted
/// from its input; a `ToolResult` carries its `tool_use_id` (resolved against
/// the id->path index to attribute a path for supersession).
#[derive(Debug, Clone, Copy)]
enum ScanUnitKind<'a> {
    ToolUse { id: &'a str, path: Option<&'a str> },
    ToolResult { tool_use_id: &'a str },
}

/// One elidable payload unit encountered by the near-lossless walk. Carries
/// everything the classification phases need without re-serializing; borrows
/// the tool-linkage strings from the request (no per-unit heap allocation).
struct ScanUnit<'a> {
    message_index: usize,
    part_index: usize,
    /// Estimated tokens of the ORIGINAL payload (before placeholder).
    tokens: u64,
    /// FNV-1a hash of the WHOLE serialized payload unit (the content-hash
    /// index key; grouping is O(1) and collisions are resolved by comparing
    /// [`Self::serialized`]).
    hash: u64,
    /// The whole-unit JSON serialization. Compared byte-for-byte on a hash
    /// match so dedup is EXACT (a hash collision can never cause a false
    /// elision).
    serialized: String,
    /// Which content part (and tool linkage) this unit came from.
    kind: ScanUnitKind<'a>,
}

/// Elidable target of a content part for the near-lossless pass, paired with
/// its tool-linkage [`ScanUnitKind`]. Mirrors [`elidable_part_tokens`]'s
/// eligibility (only `ToolResult.content` and `ToolUse.input`, never a part
/// already equal to the placeholder). A `ToolUse`'s path is extracted eagerly
/// from its input via [`extract_path`]; a `ToolResult` carries its
/// `tool_use_id` for later id->path resolution.
fn near_lossless_unit(part: &ContentPart) -> Option<(&Value, ScanUnitKind<'_>)> {
    let ContentPart::Known(known) = part else {
        return None;
    };
    let (target, kind) = match known {
        KnownContentPart::ToolResult {
            content,
            tool_use_id,
            ..
        } => (
            content,
            ScanUnitKind::ToolResult {
                tool_use_id: tool_use_id.as_str(),
            },
        ),
        KnownContentPart::ToolUse { input, id, .. } => (
            input,
            ScanUnitKind::ToolUse {
                id: id.as_str(),
                path: extract_path(input),
            },
        ),
        _ => return None,
    };
    if is_placeholder(target) {
        return None;
    }
    Some((target, kind))
}

/// Extract a file path from a `ToolUse.input` value using ONLY the
/// conservative [`PATH_KEYS`] set. Returns a borrow into `input` (no per-unit
/// heap allocation -- the pass runs on the dispatch hot path once the
/// recorder wires it in). Fail-closed: a non-object input, a missing key, a
/// non-string value,
/// or an empty string all yield `None` (no path -> a paired result cannot be
/// superseded). Fixed order: `file_path` takes precedence over `path`.
fn extract_path(input: &Value) -> Option<&str> {
    let obj = input.as_object()?;
    for key in PATH_KEYS {
        match obj.get(key) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.as_str()),
            _ => {}
        }
    }
    None
}

/// Collect near-lossless (dedup + plain supersession) elision marks over the
/// `[start, scan_end)` message window, as a pure function of request content.
///
/// SIBLING to [`collect_elision_marks`]; the shipped size-baseline path is
/// left byte-untouched so `would_trim_tokens` keeps its meaning across the
/// deploy boundary. Bounds are passed as ARGS (config is resolved by the
/// caller); the window uses the same front-anchored discipline as the
/// baseline scan.
///
/// One O(parts) forward walk ([`collect_scan_units`]) serializes each elidable
/// payload unit once (FNV-1a whole-unit hash + byte-exact key) and builds an
/// `id -> path` index from every path-bearing `ToolUse`. Classification then
/// runs in a FIXED order:
/// 1. SUPERSESSION (path-keyed): a `ToolResult` is attributed a path by
///    resolving its `tool_use_id` against the id->path index (Anthropic-shape
///    `ToolUse.id` <-> `ToolResult.tool_use_id` linkage). Within a same-path
///    group of results, elide every earlier RESULT whose content differs from
///    the group's LATEST result (keep later, elide older). The `ToolUse` call
///    block + its input always SURVIVE -- only stale RESULT content is marked.
///    A result with no resolvable path (no paired call, or the call yields no
///    [`PATH_KEYS`] entry) is fail-closed (never marked).
/// 2. DEDUP (content-keyed, over the SURVIVORS): exact whole-unit byte
///    equality; keep the FIRST copy, elide later copies. Any byte difference
///    is a hash miss -> KEEP.
///
/// SCOPE (mirrors the shipped `collect_elision_marks`, content-parts only):
/// only `MessageContent::Parts` `ToolUse` / `ToolResult` are considered.
/// OpenAI-shape tool linkage (`Message.tool_calls` + a `Role::Tool` message's
/// `tool_call_id`) is NOT handled here, exactly as the baseline path does not
/// handle it -- keeping `would_trim` comparable across the deploy boundary.
///
/// Marks are OVERLAP-DEDUPED (a superseded unit is never also deduped) and
/// returned sorted by `(message_index, part_index)`. Every doubt fails closed
/// to KEEP. See [`NearLosslessMarks`] for the field contract.
#[must_use]
pub fn collect_near_lossless_marks(
    req: &ChatRequest,
    start: usize,
    scan_end: usize,
) -> NearLosslessMarks {
    let scan_end = scan_end.min(req.messages.len());
    if start >= scan_end {
        return NearLosslessMarks::default();
    }

    let (units, id_to_path) = collect_scan_units(req, start, scan_end);

    let mut kinds = vec![MarkKind::None; units.len()];
    let (path_units, path_extractable) = classify_supersession(&units, &id_to_path, &mut kinds);
    classify_dedup_survivors(&units, &mut kinds);
    let (marks, dedup_tokens, supersession_tokens) = emit_sorted_marks(&units, &kinds);

    NearLosslessMarks {
        marks,
        dedup_tokens,
        supersession_tokens,
        path_units,
        path_extractable,
    }
}

/// ONE forward O(parts) walk over `[start, scan_end)`: serialize each elidable
/// payload unit ONCE (whole-unit hash + byte-exact key) and, for every
/// path-bearing `ToolUse`, record its `id -> path` mapping. A result's paired
/// `ToolUse` precedes it in a valid transcript, so the id->path entry already
/// exists when supersession later resolves a result's `tool_use_id`.
fn collect_scan_units(
    req: &ChatRequest,
    start: usize,
    scan_end: usize,
) -> (Vec<ScanUnit<'_>>, HashMap<&str, &str>) {
    let mut units: Vec<ScanUnit> = Vec::new();
    let mut id_to_path: HashMap<&str, &str> = HashMap::new();

    for message_index in start..scan_end {
        let MessageContent::Parts(parts) = &req.messages[message_index].content else {
            continue;
        };
        for (part_index, part) in parts.iter().enumerate() {
            let Some((target, kind)) = near_lossless_unit(part) else {
                continue;
            };
            let serialized = serde_json::to_string(target).unwrap_or_default();
            let hash = fnv1a_hash(serialized.as_bytes());
            // Reuse the byte length already computed for the hash: this is
            // identical to estimate_value_tokens(target) but serializes once.
            let tokens = estimate_str_tokens(&serialized);
            // First path-bearing call for an id wins (ids are unique in a valid
            // transcript; first-wins keeps the index deterministic regardless).
            if let ScanUnitKind::ToolUse { id, path: Some(p) } = kind {
                id_to_path.entry(id).or_insert(p);
            }
            units.push(ScanUnit {
                message_index,
                part_index,
                tokens,
                hash,
                serialized,
                kind,
            });
        }
    }

    (units, id_to_path)
}

/// Phase 1 -- SUPERSESSION (path-keyed), FIRST. Attribute each in-window
/// `ToolResult` a path by resolving its `tool_use_id` against the id->path
/// index; group results by resolved path (append order preserves transcript
/// order). Within a group of 2+, elide every earlier RESULT whose content
/// differs from the group's LATEST result (keep later / elide older). A result
/// with no resolvable path is fail-closed (never marked); the `ToolUse` call
/// blocks are never touched. Returns the path-attribution count-pair
/// `(attempted, resolved)` measured over RESULT parts.
fn classify_supersession(
    units: &[ScanUnit],
    id_to_path: &HashMap<&str, &str>,
    kinds: &mut [MarkKind],
) -> (u64, u64) {
    let mut path_units: u64 = 0;
    let mut path_extractable: u64 = 0;
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();

    for (i, unit) in units.iter().enumerate() {
        let ScanUnitKind::ToolResult { tool_use_id } = unit.kind else {
            continue;
        };
        path_units += 1;
        if let Some(&path) = id_to_path.get(tool_use_id) {
            path_extractable += 1;
            groups.entry(path).or_default().push(i);
        }
    }

    // Each result belongs to exactly one path group, so the decision is
    // order-independent (iteration over the group map cannot affect outcome).
    for group in groups.values() {
        let Some((&last, earlier)) = group.split_last() else {
            continue;
        };
        let last_hash = units[last].hash;
        for &i in earlier {
            if units[i].hash != last_hash {
                kinds[i] = MarkKind::Supersession;
            }
        }
    }

    (path_units, path_extractable)
}

/// Phase 2 -- DEDUP (content-keyed), over the SURVIVORS only. Iterate in
/// transcript order so the FIRST occurrence survives; a later unit whose
/// whole-unit bytes exactly equal an earlier survivor is elided. The hash
/// buckets grouping; byte comparison makes it exact (a hash collision can
/// never cause a false elision). Superseded units are excluded (overlap-dedup).
fn classify_dedup_survivors(units: &[ScanUnit], kinds: &mut [MarkKind]) {
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for i in 0..units.len() {
        if kinds[i] != MarkKind::None {
            continue;
        }
        let bucket = buckets.entry(units[i].hash).or_default();
        let is_duplicate = bucket
            .iter()
            .any(|&first| units[first].serialized == units[i].serialized);
        if is_duplicate {
            kinds[i] = MarkKind::Dedup;
        } else {
            bucket.push(i);
        }
    }
}

/// Emit marks in unit order (already sorted by `(message_index, part_index)`)
/// with per-heuristic token attribution. Returns `(marks, dedup_tokens,
/// supersession_tokens)`.
fn emit_sorted_marks(units: &[ScanUnit], kinds: &[MarkKind]) -> (Vec<ElisionMark>, u64, u64) {
    let mut marks = Vec::new();
    let mut dedup_tokens: u64 = 0;
    let mut supersession_tokens: u64 = 0;
    for (i, unit) in units.iter().enumerate() {
        match kinds[i] {
            MarkKind::Supersession => {
                supersession_tokens = supersession_tokens.saturating_add(unit.tokens);
                marks.push(near_lossless_mark(unit));
            }
            MarkKind::Dedup => {
                dedup_tokens = dedup_tokens.saturating_add(unit.tokens);
                marks.push(near_lossless_mark(unit));
            }
            MarkKind::None => {}
        }
    }
    (marks, dedup_tokens, supersession_tokens)
}

/// Build an M1 [`ElisionMark`] (plain placeholder, `replacement: None`) from a
/// scanned unit.
const fn near_lossless_mark(unit: &ScanUnit<'_>) -> ElisionMark {
    ElisionMark {
        message_index: unit.message_index,
        part_index: unit.part_index,
        original_tokens: unit.tokens,
        replacement: None,
    }
}

/// Build a [`PrefixReductionCandidate`] from near-lossless marks using the
/// SAME token-estimation helpers as the shipped [`propose_steady_state_trim`]
/// path, so the near-lossless candidate prices through the UNCHANGED
/// [`crate::cost_gate::evaluate`] / [`crate::cost_gate::break_even_k`].
///
/// `d` = summed original payload tokens minus the placeholder tokens that
/// replace each mark (M1 marks are `None` -> the fixed placeholder). `c_after`
/// = the cached-token footprint from the FIRST marked message to the end of
/// the request (the one-time re-write suffix). `c` = the whole-request token
/// estimate. Returns `None` when there is nothing to price (no marks).
#[must_use]
pub fn near_lossless_candidate(
    req: &ChatRequest,
    marks: &[ElisionMark],
) -> Option<PrefixReductionCandidate> {
    let elided_tokens: u64 = marks.iter().map(|m| m.original_tokens).sum();
    let placeholder_tokens = estimate_str_tokens(ELISION_PLACEHOLDER);
    // M1 marks carry `replacement: None`, so every mark is replaced by the
    // fixed placeholder -- mirrors `propose_steady_state_trim`'s d-math.
    let replaced = placeholder_tokens.saturating_mul(marks.len() as u64);
    let d = elided_tokens.saturating_sub(replaced);

    // `c_after` = the cached-token footprint from the FIRST marked position to
    // the end of the request (the one-time re-write suffix); `c` = the whole
    // request. Same shared helper as the shipped path -- returns `None` (with
    // this whole builder) when there is nothing to price.
    let c_after = c_after_from_first_mark(req, marks)?;
    let c = estimate_total_tokens(req);

    Some(PrefixReductionCandidate::new(d, c_after, c))
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
            assert_eq!(fp_base, fp_g, "fingerprint drifted at growth step {growth}");
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

    #[test]
    fn apply_trim_plan_substitutes_custom_replacement() {
        // Arrange: a real plan, with its first mark's replacement overridden
        // to a custom string (the M3 path-bearing-placeholder shape).
        let req = long_conversation(6, 12_000);
        let params = SteadyStateTrimParams::default();
        let mut plan = propose_steady_state_trim(&req, &params).expect("plan");
        let custom = "[elided: replaced by custom marker]";
        plan.marks[0].replacement = Some(custom.into());

        // Act
        let trimmed = apply_trim_plan(&req, &plan);

        // Assert: the customized mark carries the custom string; every other
        // mark still carries the fixed placeholder.
        for (i, mark) in plan.marks.iter().enumerate() {
            let parts = match &trimmed.messages[mark.message_index].content {
                MessageContent::Parts(p) => p,
                other => panic!("expected parts, got {other:?}"),
            };
            let payload = tool_payload(&parts[mark.part_index]);
            let expected = if i == 0 { custom } else { ELISION_PLACEHOLDER };
            assert_eq!(payload, Some(&json!(expected)));
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
        use crate::catalog::lookup;
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

    /// A conversation where non-elidable TEXT turns sit between the fixed head
    /// and the first elidable tool turn, so the first elision mark lands at a
    /// message index strictly greater than `head_keep_messages`.
    fn conversation_with_text_gap_before_tools() -> ChatRequest {
        let payload = payload_of_tokens(12_000);
        let mut messages = vec![
            user_msg("head one"),      // 0 (head)
            assistant_msg("head two"), // 1 (head)
            user_msg("plain turn a"),  // 2 non-elidable text
            assistant_msg("plain b"),  // 3
            user_msg("plain turn c"),  // 4
        ];
        for _ in 0..8 {
            messages.push(tool_use_msg(&payload)); // first at index 5
            messages.push(tool_result_msg(&payload));
        }
        for i in 0..6 {
            messages.push(user_msg(&format!("recent turn {i}")));
        }
        ChatRequest {
            model: "claude-opus-4-8".into(),
            messages,
            ..Default::default()
        }
    }

    #[test]
    fn c_after_starts_at_first_mark_not_head_keep() {
        // Arrange: head_keep=2, but the first elidable mark sits at index 5.
        let req = conversation_with_text_gap_before_tools();
        let params = SteadyStateTrimParams::default();

        // Act
        let plan = propose_steady_state_trim(&req, &params).expect("plan");

        // Assert: c_after is priced from the FIRST elided message, not the
        // fixed head boundary. The text turns at [head_keep, first_mark) must
        // not be counted into the one-time rewrite suffix.
        let n = req.messages.len();
        let first_mark = plan
            .marks
            .iter()
            .map(|m| m.message_index)
            .min()
            .expect("marks");
        assert_eq!(first_mark, 5, "fixture places first mark after head_keep");
        assert_eq!(
            plan.candidate.c_after,
            estimate_messages_tokens(&req, first_mark..n)
        );
        // The head-boundary count is strictly larger (the gap turns add tokens),
        // so the old span_start-based c_after was an overstatement.
        assert!(
            estimate_messages_tokens(&req, params.head_keep_messages..n) > plan.candidate.c_after
        );
    }

    #[test]
    fn both_builders_agree_on_c_after_for_identical_marks() {
        // Arrange: identical marks fed to both trim-candidate builders.
        let req = conversation_with_text_gap_before_tools();
        let params = SteadyStateTrimParams::default();
        let plan = propose_steady_state_trim(&req, &params).expect("plan");

        // Act: price the SAME marks through the sibling builder.
        let sibling = near_lossless_candidate(&req, &plan.marks).expect("candidate");

        // Assert: the shared helper makes both c_after values identical -- a
        // tripwire that a future third computation path must also satisfy.
        assert_eq!(plan.candidate.c_after, sibling.c_after);
    }

    // ================================================================
    // NEAR-LOSSLESS MARKS (collect_near_lossless_marks) -- dedup + plain
    // supersession. SIBLING to the shipped baseline path above.
    // ================================================================

    /// A `ChatRequest` over the given messages, with a stable model id.
    fn req_of(messages: Vec<Message>) -> ChatRequest {
        ChatRequest {
            model: "claude-opus-4-8".into(),
            messages,
            ..Default::default()
        }
    }

    /// An assistant `tool_use` turn with an explicit `id` and JSON `input`.
    /// The `id` links to a paired `tool_result`'s `tool_use_id`
    /// (Anthropic-shape content-part pairing), letting a test build distinct
    /// linked call/result pairs.
    fn tool_use_of(id: &str, input: Value) -> Message {
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: id.into(),
                name: "Tool".into(),
                input,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// A user `tool_result` turn linked to `tool_use_id`, carrying JSON
    /// `content`. Pairs with [`tool_use_of`] via the shared id.
    fn tool_result_of(tool_use_id: &str, content: Value) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: tool_use_id.into(),
                    content,
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

    /// A user `tool_result` turn carrying an arbitrary JSON `content`.
    fn tool_result_content(content: Value) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: "toolu_x".into(),
                    content,
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

    // -- DEDUP ------------------------------------------------------------

    #[test]
    fn dedup_keeps_first_elides_later_identical_tool_results() {
        // Arrange: two byte-identical tool_result payloads in the window.
        let payload = json!(payload_of_tokens(3_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(payload.clone()), // idx 1: first (kept)
            assistant_msg("mid"),
            tool_result_content(payload.clone()), // idx 3: later (elided)
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: exactly one dedup mark, on the LATER copy; first survives.
        assert_eq!(out.marks.len(), 1);
        assert_eq!(out.marks[0].message_index, 3);
        assert_eq!(out.marks[0].part_index, 0);
        assert_eq!(out.marks[0].replacement, None);
        assert!(out.dedup_tokens > 0);
        assert_eq!(out.supersession_tokens, 0);
        assert_eq!(out.dedup_tokens, out.marks[0].original_tokens);
    }

    #[test]
    fn one_byte_difference_is_not_deduped() {
        // Arrange: two payloads of equal length differing in exactly one byte.
        let a = json!("x".repeat(4_000));
        let b = json!(format!("{}y", "x".repeat(3_999)));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(a),
            tool_result_content(b),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: any byte difference is a hash miss -> KEEP.
        assert!(
            out.marks.is_empty(),
            "one-byte-different payloads must not dedup"
        );
        assert_eq!(out.dedup_tokens, 0);
    }

    #[test]
    fn diff_quoting_file_lines_does_not_match_file_read_unit() {
        // Arrange: a file-read result, and a later diff result that QUOTES
        // those same lines (a superset). Whole-unit hashing must not match a
        // sub-payload overlap.
        let file_text = "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n".repeat(80);
        let file_read = json!(file_text.clone());
        let diff = json!(format!(
            "@@ -1,3 +1,3 @@\n-    let x = 1;\n+    let x = 2;\n{file_text}"
        ));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(file_read),
            tool_result_content(diff),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert
        assert!(out.marks.is_empty(), "sub-payload overlap must not dedup");
        assert_eq!(out.dedup_tokens, 0);
    }

    #[test]
    fn dedup_marks_are_attributed_to_dedup_tokens_only() {
        // Arrange: three identical tool_result payloads -> two dedup marks.
        let payload = json!(payload_of_tokens(2_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(payload.clone()), // idx1 first (kept)
            tool_result_content(payload.clone()), // idx2 dup
            tool_result_content(payload.clone()), // idx3 dup
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: first copy kept, two later copies elided by dedup.
        let positions: Vec<usize> = out.marks.iter().map(|m| m.message_index).collect();
        assert_eq!(positions, vec![2, 3]);
        let expected: u64 = out.marks.iter().map(|m| m.original_tokens).sum();
        assert_eq!(out.dedup_tokens, expected);
        assert_eq!(out.supersession_tokens, 0);
    }

    // -- SUPERSESSION (RESULT-side; Anthropic-shape ToolUse <-> ToolResult) -

    #[test]
    fn supersession_elides_older_result_when_later_same_path_differs() {
        // Arrange: read /a (call t1) -> result A, then edit /a (call t2) ->
        // result B. Both RESULTS resolve to path /a via their paired ToolUse
        // (id <-> tool_use_id); different content -> the OLDER result content
        // is superseded, the later kept, and BOTH call blocks survive intact.
        let a = json!("A".repeat(12_000));
        let b = json!("B".repeat(12_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "kind": "read"})), // idx1 call (survives)
            tool_result_of("t1", a), // idx2 older result (superseded)
            tool_use_of("t2", json!({"file_path": "/a", "kind": "edit"})), // idx3 call (survives)
            tool_result_of("t2", b), // idx4 later result (kept)
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: exactly one mark, on the OLDER RESULT (idx2), never a call.
        assert_eq!(out.marks.len(), 1);
        assert_eq!(
            out.marks[0].message_index, 2,
            "the older RESULT content is elided, not the call block"
        );
        assert_eq!(out.marks[0].part_index, 0);
        assert_eq!(out.marks[0].replacement, None);
        assert!(out.supersession_tokens > 0);
        assert_eq!(out.dedup_tokens, 0);
        assert_eq!(out.supersession_tokens, out.marks[0].original_tokens);
        // The count-pair now measures RESULT parts: two results, both resolved.
        assert_eq!(out.path_units, 2);
        assert_eq!(out.path_extractable, 2);
    }

    #[test]
    fn identical_same_path_results_are_dedup_not_supersession() {
        // Arrange: two reads of /a (calls t1, t2 with DISTINCT inputs so the
        // calls never dedup) producing IDENTICAL result content. Same path +
        // same content = DEDUP's domain (keep first), not supersession (which
        // needs a DIFFERENT later content).
        let x = json!(payload_of_tokens(3_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "seq": 1})),
            tool_result_of("t1", x.clone()), // idx2 first result (kept)
            tool_use_of("t2", json!({"file_path": "/a", "seq": 2})),
            tool_result_of("t2", x.clone()), // idx4 identical result (deduped)
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: division of labor -- the exact copy is dedup's, not supersession's.
        assert_eq!(out.marks.len(), 1);
        assert_eq!(out.marks[0].message_index, 4);
        assert!(out.dedup_tokens > 0);
        assert_eq!(out.supersession_tokens, 0);
    }

    #[test]
    fn supersession_takes_precedence_over_dedup_and_each_unit_marked_once() {
        // Arrange: path /a with RESULT contents v1, v2, v1 (idx6 == idx2). The
        // latest result (idx6, v1) is the survivor. idx4 (v2) differs from it
        // -> supersession. idx2 (v1) equals it -> survives supersession; dedup
        // then elides the later identical copy (idx6), keeping idx2. Calls use
        // DISTINCT inputs so the call blocks never dedup.
        let v1 = json!("V1".repeat(2_000));
        let v2 = json!("V2".repeat(2_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "call": 1})),
            tool_result_of("t1", v1.clone()), // idx2 v1
            tool_use_of("t2", json!({"file_path": "/a", "call": 2})),
            tool_result_of("t2", v2), // idx4 v2 -> superseded
            tool_use_of("t3", json!({"file_path": "/a", "call": 3})),
            tool_result_of("t3", v1.clone()), // idx6 v1 -> deduped (copy of idx2)
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: idx4 superseded, idx6 deduped; sorted; each marked once.
        let positions: Vec<usize> = out.marks.iter().map(|m| m.message_index).collect();
        assert_eq!(positions, vec![4, 6]);
        let unique: std::collections::HashSet<_> = out
            .marks
            .iter()
            .map(|m| (m.message_index, m.part_index))
            .collect();
        assert_eq!(
            unique.len(),
            out.marks.len(),
            "each unit marked by at most one heuristic"
        );
        assert!(out.supersession_tokens > 0);
        assert!(out.dedup_tokens > 0);
    }

    #[test]
    fn supersession_keeps_older_result_matching_current_latest() {
        // Arrange: read /a -> A, edit /a -> B, read /a -> A again (reverted).
        // The first result (idx2, A) matches the LATEST (idx6, A), so it is
        // NOT superseded; the stale middle edit's result (idx4, B) is. Dedup
        // then keeps idx2, elides the later identical idx6.
        let a = json!("A".repeat(4_000));
        let b = json!("B".repeat(4_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "op": 1})),
            tool_result_of("t1", a.clone()), // idx2 A (matches latest -> kept)
            tool_use_of("t2", json!({"file_path": "/a", "op": 2})),
            tool_result_of("t2", b), // idx4 B -> superseded
            tool_use_of("t3", json!({"file_path": "/a", "op": 3})),
            tool_result_of("t3", a.clone()), // idx6 A -> deduped (copy of idx2)
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: the older result equal to the latest survives supersession.
        let positions: Vec<usize> = out.marks.iter().map(|m| m.message_index).collect();
        assert_eq!(positions, vec![4, 6]);
        assert!(
            !positions.contains(&2),
            "the older result equal to the latest is not superseded"
        );
        assert!(out.supersession_tokens > 0);
        assert!(out.dedup_tokens > 0);
    }

    // -- ORDERING ---------------------------------------------------------

    #[test]
    fn marks_are_sorted_by_message_and_part_index() {
        // Arrange: two interleaved dedup groups across several messages.
        let p = json!("z".repeat(2_000));
        let q = json!("w".repeat(2_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(p.clone()), // idx1 first p
            tool_result_content(q.clone()), // idx2 first q
            tool_result_content(p.clone()), // idx3 dup p
            tool_result_content(q.clone()), // idx4 dup q
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: sorted ascending by (message_index, part_index).
        let keys: Vec<(usize, usize)> = out
            .marks
            .iter()
            .map(|m| (m.message_index, m.part_index))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
        assert_eq!(keys, vec![(3, 0), (4, 0)]);
    }

    // -- PATH ATTRIBUTION / COUNTERS (result-keyed) -----------------------

    #[test]
    fn path_extraction_recognizes_both_file_path_and_path_keys() {
        // Arrange: the paired calls key their path differently -- t1 via
        // `file_path`, t2 via `path` -- both resolving to "/a". Both keys are
        // recognized when building the id->path index, so both RESULTS resolve
        // and the older (different) result is superseded.
        let a = json!("A".repeat(4_000));
        let b = json!("B".repeat(4_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "n": 1})),
            tool_result_of("t1", a), // idx2 older result -> superseded
            tool_use_of("t2", json!({"path": "/a", "n": 2})),
            tool_result_of("t2", b), // idx4 later result -> kept
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert
        assert_eq!(out.path_units, 2);
        assert_eq!(out.path_extractable, 2);
        assert_eq!(out.marks.len(), 1);
        assert_eq!(out.marks[0].message_index, 2);
        assert!(out.supersession_tokens > 0);
    }

    #[test]
    fn result_whose_paired_call_yields_no_key_gets_no_supersession_mark() {
        // Arrange: paired calls carry an UNRECOGNIZED key ("filename"), so the
        // id->path index stays empty and neither RESULT resolves to a path --
        // fail-closed, no supersession, even though the results differ.
        let a = json!("A".repeat(4_000));
        let b = json!("B".repeat(4_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"filename": "/a", "v": 1})),
            tool_result_of("t1", a),
            tool_use_of("t2", json!({"filename": "/a", "v": 2})),
            tool_result_of("t2", b),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: fail-closed -- no resolvable path means no supersession.
        assert!(out.marks.is_empty());
        assert_eq!(out.supersession_tokens, 0);
        assert_eq!(out.path_units, 2, "both results attempt path attribution");
        assert_eq!(out.path_extractable, 0, "no paired call yielded a key");
    }

    #[test]
    fn result_without_a_paired_tool_use_gets_no_supersession_mark() {
        // Arrange: two RESULTS whose tool_use_ids have NO paired ToolUse in the
        // window. No path can be resolved -> fail-closed (no supersession).
        let a = json!("A".repeat(4_000));
        let b = json!("B".repeat(4_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_result_of("orphan1", a),
            tool_result_of("orphan2", b),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert
        assert!(out.marks.is_empty());
        assert_eq!(out.supersession_tokens, 0);
        assert_eq!(out.path_units, 2, "both results attempt path attribution");
        assert_eq!(out.path_extractable, 0, "no paired call exists to resolve");
    }

    #[test]
    fn fail_closed_non_object_non_string_and_empty_paths_not_extracted() {
        // Arrange: three paired calls whose inputs each fail path extraction --
        // a non-object input, a numeric path value, and an empty-string path.
        // None populate the id->path index, so no RESULT resolves to a path.
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!("not an object")),
            tool_result_of("t1", json!("A".repeat(2_000))),
            tool_use_of("t2", json!({"file_path": 123})),
            tool_result_of("t2", json!("B".repeat(2_000))),
            tool_use_of("t3", json!({"file_path": ""})),
            tool_result_of("t3", json!("C".repeat(2_000))),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert
        assert_eq!(out.path_units, 3, "three RESULT parts attempt attribution");
        assert_eq!(out.path_extractable, 0);
        assert_eq!(out.supersession_tokens, 0);
    }

    #[test]
    fn path_counters_count_result_parts_not_tool_use_units() {
        // Arrange: one path-bearing call paired with a result, plus a second
        // result whose call is absent. path_units counts RESULT parts (2), not
        // the ToolUse population; path_extractable counts the ones resolved (1).
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a"})),
            tool_result_of("t1", json!("some output")), // resolves via t1
            tool_result_of("t2", json!("other output")), // no paired call
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert
        assert_eq!(out.path_units, 2, "counts RESULT parts, not tool_use units");
        assert_eq!(out.path_extractable, 1, "only the paired result resolved");
    }

    // -- WINDOW DISCIPLINE ------------------------------------------------

    #[test]
    fn window_excludes_units_outside_start_and_scan_end() {
        // Arrange: four identical payloads; only [1, 3) is scanned.
        let p = json!("k".repeat(2_000));
        let req = req_of(vec![
            tool_result_content(p.clone()), // idx0 before start
            tool_result_content(p.clone()), // idx1 in window (first)
            tool_result_content(p.clone()), // idx2 in window (dup)
            tool_result_content(p.clone()), // idx3 after scan_end
        ]);

        // Act: window [1, 3) sees only idx1 and idx2.
        let out = collect_near_lossless_marks(&req, 1, 3);

        // Assert: idx1 kept (first in window), idx2 deduped; idx0/idx3 ignored.
        assert_eq!(out.marks.len(), 1);
        assert_eq!(out.marks[0].message_index, 2);
    }

    #[test]
    fn scan_end_past_len_is_clamped_and_start_ge_end_is_empty() {
        let p = json!("k".repeat(2_000));
        let req = req_of(vec![
            tool_result_content(p.clone()),
            tool_result_content(p.clone()),
        ]);

        // Over-long scan_end is clamped to len (no panic, dup still found).
        let clamped = collect_near_lossless_marks(&req, 0, 999);
        assert_eq!(clamped.marks.len(), 1);
        assert_eq!(clamped.marks[0].message_index, 1);

        // Empty window yields the default (no marks).
        let empty = collect_near_lossless_marks(&req, 2, 2);
        assert_eq!(empty, NearLosslessMarks::default());
    }

    // -- EMPTY / DETERMINISM ----------------------------------------------

    #[test]
    fn text_only_conversation_yields_empty_result() {
        let req = req_of(vec![user_msg("a"), assistant_msg("b"), user_msg("c")]);
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());
        assert_eq!(out, NearLosslessMarks::default());
    }

    #[test]
    fn placeholder_payloads_are_not_re_elided() {
        // Arrange: a part already equal to the fixed placeholder is inert.
        let ph = json!(ELISION_PLACEHOLDER);
        let req = req_of(vec![
            user_msg("head"),
            tool_result_content(ph.clone()),
            tool_result_content(ph.clone()),
            user_msg("tail"),
        ]);

        // Act
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: idempotence guard -- placeholders are never marked.
        assert_eq!(out, NearLosslessMarks::default());
    }

    #[test]
    fn same_request_twice_yields_identical_near_lossless_result() {
        // Arrange: a fixture exercising BOTH heuristics -- a superseded /a
        // result (calls t1/t2, different content) plus a dedup pair of orphan
        // results (identical content, no paired call).
        let a = json!("A".repeat(3_000));
        let b = json!("B".repeat(3_000));
        let dup = json!("d".repeat(3_000));
        let req = req_of(vec![
            user_msg("head"),
            tool_use_of("t1", json!({"file_path": "/a", "k": "read"})),
            tool_result_of("t1", a), // idx2 older /a result -> superseded
            tool_use_of("t2", json!({"file_path": "/a", "k": "edit"})),
            tool_result_of("t2", b), // idx4 latest /a result -> kept
            tool_result_of("t3", dup.clone()), // idx5 first dup -> kept
            tool_result_of("t4", dup.clone()), // idx6 dup -> deduped
            user_msg("tail"),
        ]);

        // Act
        let first = collect_near_lossless_marks(&req, 0, req.messages.len());
        let second = collect_near_lossless_marks(&req, 0, req.messages.len());

        // Assert: deterministic; and the fixture really uses both heuristics.
        assert_eq!(first, second);
        assert!(first.supersession_tokens > 0 && first.dedup_tokens > 0);
    }

    // -- CANDIDATE / PRICING ----------------------------------------------

    #[test]
    fn near_lossless_candidate_is_none_when_no_marks() {
        let req = req_of(vec![user_msg("a")]);
        assert_eq!(near_lossless_candidate(&req, &[]), None);
    }

    #[test]
    fn near_lossless_candidate_d_matches_elided_minus_placeholder() {
        // Arrange: a dedup pair; d = sum(original) - placeholder * count.
        let payload = json!(payload_of_tokens(5_000));
        let req = req_of(vec![
            user_msg("head one"),
            assistant_msg("head two"),
            tool_result_content(payload.clone()),
            user_msg("mid"),
            tool_result_content(payload.clone()),
            user_msg("tail"),
        ]);
        let out = collect_near_lossless_marks(&req, 0, req.messages.len());
        assert!(!out.marks.is_empty());

        // Act
        let candidate = near_lossless_candidate(&req, &out.marks).expect("candidate");

        // Assert
        let elided: u64 = out.marks.iter().map(|m| m.original_tokens).sum();
        let placeholder = estimate_str_tokens(ELISION_PLACEHOLDER);
        let expected_d = elided - placeholder * out.marks.len() as u64;
        assert_eq!(candidate.d, expected_d);
        assert!(candidate.c_after > 0);
        assert!(candidate.c >= candidate.c_after);
    }

    #[test]
    fn near_lossless_candidate_prices_through_unchanged_gate() {
        // Arrange: a big dedup pair (meaningful d) plus distinct bulky filler
        // so the total prefix c is large and c-d stays above min_prefix.
        use crate::catalog::lookup;
        use crate::cost_gate::{GateDecision, break_even_k, evaluate};

        let dup = json!(payload_of_tokens(12_000));
        let mut messages = vec![user_msg("head one"), assistant_msg("head two")];
        messages.push(tool_result_content(dup.clone())); // idx2 first (kept)
        for i in 0..4 {
            // Distinct inputs so the filler is never itself deduped.
            messages.push(tool_use_msg(&format!(
                "filler {i} {}",
                payload_of_tokens(6_000)
            )));
            messages.push(user_msg(&format!("filler turn {i}")));
        }
        messages.push(tool_result_content(dup.clone())); // later (deduped)
        for i in 0..6 {
            messages.push(user_msg(&format!("recent {i}")));
        }
        let req = req_of(messages);

        // Scan the front-anchored window (skip head, protect the recent tail).
        let scan_end = req.messages.len() - 6;
        let out = collect_near_lossless_marks(&req, 2, scan_end);
        assert!(!out.marks.is_empty(), "fixture must produce a dedup mark");
        let candidate = near_lossless_candidate(&req, &out.marks).expect("candidate");
        let row = lookup("anthropic-api", "claude-opus-4-8", Some("5m"));

        // Act: the candidate must price through the UNCHANGED gate.
        let k_star = break_even_k(&row, &candidate).expect("d > 0");
        let decision = evaluate(&row, &candidate, k_star + 1.0);

        // Assert
        assert_eq!(
            decision,
            GateDecision::Break {
                delta_tokens: candidate.d
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
