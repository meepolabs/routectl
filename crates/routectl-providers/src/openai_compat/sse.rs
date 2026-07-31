//! SSE chunk parsing and per-chunk normalization.
//!
//! `parse_event` is stateless: it handles one `data: ...` line and returns
//! `Ok(None)` for `[DONE]` / keepalives, or `Ok(Some(ChatChunk))` for
//! content chunks. Reasoning is lifted where the upstream field is simple
//! (DeepSeek, vLLM, OpenAI, OpenRouter).
//!
//! The `<think>` tag state machine for `RawThinkTag` is NOT stateless, so it
//! lives in `ThinkTagAccumulator` which the `stream()` caller owns across
//! chunks. Call `ThinkTagAccumulator::process` instead of `parse_event` when
//! the dialect is `RawThinkTag`.

use serde_json::Value;

use routectl_core::schema::{ChunkChoice, ChunkDelta, UsageDelta};
use routectl_core::{ChatChunk, Error, Result};

use super::dialect::ReasoningDialect;
use super::util::build_reasoning_detail;

// ---------------------------------------------------------------------------
// Streamed tool-call id synthesis
// ---------------------------------------------------------------------------

/// Per-stream synthesizer for missing streamed tool-call ids.
///
/// INVARIANT: every streamed OpenAI-shape `tool_call` must carry a
/// non-empty `id` so the downstream openai->anthropic pairing (which
/// keys the follow-up `tool_result` on the emitted id) is not orphaned.
/// Some upstreams stream an indexed tool_call with no id at all; this
/// synthesizes a stable one.
///
/// Scheme (matches the non-streaming `crate::tool_calls::normalize_tool_calls`
/// fallback so the two paths cannot drift): the first delta of a
/// `(choice.index, tool_call.index)` key that arrives id-less mints
/// `call_{tool_call_index}` (choice 0) or `call_{choice}_{tool_call_index}`
/// (choice > 0, so cross-choice ids never collide when `n > 1`); every
/// later delta of that key reuses it. A key whose first delta already
/// carried a non-empty id is left untouched (its later id-less argument
/// deltas keep the verbatim passthrough shape).
///
/// Every id that reaches the wire -- real upstream ids and minted ids
/// alike -- is RESERVED. A real id keyed by a usable `(choice, index)`
/// reserves against that slot; a real id whose `index` is malformed keys
/// no slot but is still reserved by value so it participates in collision
/// detection. Reservation makes two failure modes impossible:
///   - Once a slot has emitted an id, that id is STABLE: a late real id
///     arriving after a mint (or after an earlier real id) never replaces
///     the value the client already saw, so a single call never emits two
///     different ids across its deltas.
///   - A mint that would collide with an id already reserved by a
///     DIFFERENT slot picks a unique alternative instead; a real upstream
///     id that collides with a different slot's reserved id is genuinely
///     ambiguous and fails the stream rather than mispairing a
///     `tool_result` onto the wrong call.
#[derive(Debug, Default)]
pub(crate) struct StreamedToolCallIds {
    slots: std::collections::HashMap<(u32, u32), IdSlot>,
    /// Every id emitted on the wire -> the slot that owns it. `Some(key)`
    /// is a keyed `(choice, index)` slot (real or minted); `None` is a
    /// real upstream id whose `index` was malformed, so it has no slot
    /// key but must still block a later mint and fail a later keyed id
    /// that would duplicate it. Guards against a mint or a real id
    /// colliding with a different slot's id.
    reserved: std::collections::HashMap<String, Option<(u32, u32)>>,
    /// Monotonic disambiguation counter for the rare mint collision.
    mint_seq: u64,
}

#[derive(Debug)]
enum IdSlot {
    /// The first delta for this key carried a non-empty upstream id,
    /// stored so a late delta carrying a DIFFERENT id is overwritten back
    /// to the value the client already saw.
    Upstream(String),
    /// No upstream id arrived first; this id was minted and is reused.
    Minted(String),
}

impl StreamedToolCallIds {
    /// Fill in a stable id on every id-less streamed `tool_call` in
    /// `chunk`, mutating the chunk in place before it is yielded.
    ///
    /// Returns `Err(Error::Streaming)` when a tool_call cannot be paired
    /// safely: an id-less call whose `index` is not a valid `u32`, or a
    /// real upstream id that collides with an id already reserved by a
    /// different slot. A hard stream error beats routing a `tool_result`
    /// to the wrong call.
    pub(crate) fn fill_missing_ids(
        &mut self,
        provider_id: &str,
        chunk: &mut ChatChunk,
    ) -> Result<()> {
        for choice in &mut chunk.choices {
            let choice_index = choice.index;
            let Some(tool_calls) = choice.delta.tool_calls.as_mut() else {
                continue;
            };
            for tc in tool_calls.iter_mut() {
                let index = parse_tool_call_index(tc);
                let real_id = tc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);

                if let Some(id) = real_id {
                    // A tool_call that already carries an id is untouched
                    // regardless of `index`. A usable index keys a slot; a
                    // malformed index cannot key a slot but the id still
                    // reaches the wire, so reserve it by value to keep it in
                    // collision detection (block a later mint, fail a later
                    // keyed id that would duplicate it).
                    match index {
                        Some(tc_index) => {
                            self.apply_real_id(provider_id, choice_index, tc_index, tc, id)?;
                        }
                        None => self.reserve_wire_id(provider_id, id)?,
                    }
                    continue;
                }

                // Id-less: the `(choice, index)` slot key must be exact --
                // a missing / negative / non-numeric / out-of-range index
                // cannot silently collapse to 0 and mispair two distinct
                // id-less calls onto the same synthesized id.
                let Some(tc_index) = index else {
                    return Err(Error::Streaming(format!(
                        "provider `{provider_id}`: id-less streamed tool_call has a \
                         missing or non-u32 `index`; cannot synthesize a stable id"
                    )));
                };
                self.apply_missing_id(provider_id, choice_index, tc_index, tc);
            }
        }
        Ok(())
    }

    /// Handle a `tool_call` delta that arrived with a non-empty upstream id.
    fn apply_real_id(
        &mut self,
        provider_id: &str,
        choice_index: u32,
        tc_index: u32,
        tc: &mut Value,
        id: String,
    ) -> Result<()> {
        let key = (choice_index, tc_index);
        match self.slots.get(&key) {
            // The slot already established an id (real or minted) that the
            // client saw; a late delta carrying a DIFFERENT id must NOT
            // replace it, so overwrite this delta back to the established id.
            Some(IdSlot::Upstream(established) | IdSlot::Minted(established)) => {
                set_tool_call_id(tc, established);
                Ok(())
            }
            // First delta for this slot. Reserve the real id, failing the
            // stream if a different slot already owns it (mispairing risk).
            None => {
                if self.reserved_by_other(&id, key) {
                    return Err(Error::Streaming(format!(
                        "provider `{provider_id}`: streamed tool_call id `{id}` collides \
                         with an id already reserved for a different call; cannot pair safely"
                    )));
                }
                self.reserved.insert(id.clone(), Some(key));
                self.slots.insert(key, IdSlot::Upstream(id));
                Ok(())
            }
        }
    }

    /// Reserve a real upstream id that arrived with a malformed `index` and
    /// so cannot key a slot. The id still reaches the wire verbatim, so it
    /// must participate in collision detection: fail the stream if a keyed
    /// slot already owns this exact id (mispairing risk); otherwise record
    /// it slot-less so a later mint disambiguates and a later keyed id that
    /// would duplicate it fails. A repeat of the same slot-less id is
    /// idempotent -- the client already saw it and it passes through
    /// unchanged.
    fn reserve_wire_id(&mut self, provider_id: &str, id: String) -> Result<()> {
        match self.reserved.get(&id) {
            Some(Some(_)) => Err(Error::Streaming(format!(
                "provider `{provider_id}`: streamed tool_call id `{id}` collides \
                 with an id already reserved for a different call; cannot pair safely"
            ))),
            Some(None) => Ok(()),
            None => {
                self.reserved.insert(id, None);
                Ok(())
            }
        }
    }

    /// Handle an id-less `tool_call` delta whose `index` is a valid `u32`.
    fn apply_missing_id(
        &mut self,
        provider_id: &str,
        choice_index: u32,
        tc_index: u32,
        tc: &mut Value,
    ) {
        let key = (choice_index, tc_index);
        match self.slots.get(&key) {
            // First delta established an upstream id; the client already has
            // it, so later id-less deltas pass through untouched.
            Some(IdSlot::Upstream(_)) => {}
            Some(IdSlot::Minted(id)) => set_tool_call_id(tc, id),
            None => {
                let id = self.mint_unique_id(choice_index, tc_index, key);
                tracing::debug!(
                    provider = %provider_id,
                    choice_index,
                    tool_call_index = tc_index,
                    generated_id = %id,
                    "openai-compat: synthesized missing streamed tool_call id"
                );
                set_tool_call_id(tc, &id);
                self.reserved.insert(id.clone(), Some(key));
                self.slots.insert(key, IdSlot::Minted(id));
            }
        }
    }

    /// True when `id` is already reserved by a slot other than `key` (a
    /// different keyed slot, or a slot-less real id).
    fn reserved_by_other(&self, id: &str, key: (u32, u32)) -> bool {
        matches!(self.reserved.get(id), Some(owner) if *owner != Some(key))
    }

    /// Mint the scheme id for the slot; if a different slot already reserved
    /// it, derive a guaranteed-unique alternative rather than colliding.
    fn mint_unique_id(&mut self, choice_index: u32, tc_index: u32, key: (u32, u32)) -> String {
        let base = mint_id(choice_index, tc_index);
        if !self.reserved_by_other(&base, key) {
            return base;
        }
        loop {
            self.mint_seq += 1;
            let candidate = format!("{base}_{}", self.mint_seq);
            if !self.reserved.contains_key(&candidate) {
                return candidate;
            }
        }
    }
}

/// Parse a streamed `tool_call`'s `index` as a `u32`. Returns `None` for a
/// missing, negative, non-numeric, or out-of-range `index` -- there is no
/// fallback to 0, so an id-less call with a bad index is rejected upstream
/// instead of colliding on slot `(choice, 0)`.
fn parse_tool_call_index(tool_call: &Value) -> Option<u32> {
    tool_call
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
}

/// Mint a stable synthesized id. Choice 0 uses the non-streaming
/// `call_{index}` form verbatim; a non-zero choice encodes the choice
/// index so cross-choice ids stay distinct when `n > 1`.
fn mint_id(choice_index: u32, tool_call_index: u32) -> String {
    if choice_index == 0 {
        format!("call_{tool_call_index}")
    } else {
        format!("call_{choice_index}_{tool_call_index}")
    }
}

fn set_tool_call_id(tool_call: &mut Value, id: &str) {
    if let Some(obj) = tool_call.as_object_mut() {
        obj.insert("id".to_string(), Value::String(id.to_string()));
    }
}

// ---------------------------------------------------------------------------
// Stateless path
// ---------------------------------------------------------------------------

/// Parse one SSE data line for dialects that do not need cross-chunk state.
///
/// `raw` is the value portion after `data: `, e.g. `{"id":"...","choices":[...]}`.
/// Returns `Ok(None)` for `[DONE]`, empty lines, and comment lines.
///
/// `reasoning_index` is a per-stream, monotonically incrementing counter
/// owned by the streaming caller (see `stream()`); dialects that lift
/// streamed reasoning thread it into each emitted detail's `index`.
pub fn parse_event(
    id: &str,
    raw: &str,
    dialect: ReasoningDialect,
    reasoning_index: &mut u32,
) -> Result<Option<ChatChunk>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
        return Ok(None);
    }

    let mut val: Value = serde_json::from_str(trimmed)
        .map_err(|e| Error::Streaming(format!("provider `{id}`: SSE JSON: {e}")))?;

    // Detect mid-stream error envelope before any normalization. Some
    // upstreams (Azure content filter, OpenRouter overload, NIM auth-wall)
    // emit a JSON error object inside a 200-OK SSE stream rather than a
    // top-level HTTP error. Treating it as a bad ChatChunk deserialize
    // would produce a misleading NormalizeResponse error.
    if let Some(err) = detect_error_envelope(id, &val) {
        return Err(err);
    }

    // Coalesce reasoning_content -> reasoning across all delta objects so
    // serde sees a single canonical key and dialect-specific lifters can
    // operate on a uniform shape.
    coalesce_chunk_reasoning_keys(&mut val);

    // Lift OpenAI/DeepSeek usage sub-bags into canonical typed slots
    // BEFORE serde deserialization. `UsageDelta` does not carry a
    // `#[serde(flatten)] extras` catchall, so unknown sub-fields are
    // silently dropped at deserialize time -- without this lift, the
    // terminal-chunk's `completion_tokens_details.reasoning_tokens`
    // and `prompt_cache_hit_tokens` / `prompt_tokens_details
    // .cached_tokens` would never reach canonical.
    lift_chunk_usage_subbags(&mut val);

    apply_chunk_dialect(id, &mut val, dialect, reasoning_index)?;

    let chunk: ChatChunk = serde_json::from_value(val)
        .map_err(|e| Error::normalize_response(id, format!("chunk deserialize: {e}")))?;

    Ok(Some(chunk))
}

/// Detect a mid-stream JSON error envelope (`{"error":{...}}`) emitted
/// inside a 200-OK SSE stream by some upstreams (Azure content filter,
/// OpenRouter overload, NIM auth-wall). Returns `Some(Error::Upstream)`
/// when the envelope is present so the caller can short-circuit instead
/// of treating it as a malformed ChatChunk. A `null` or empty-object
/// `error` field is not an envelope and returns `None`. Status defaults
/// to 502 when `error.code` is absent or non-numeric; message defaults
/// to a generic string. The upstream `error.type` / `error.code`
/// classifier is lifted onto the error so the surfaced fault matches what
/// the non-streaming path carries. Shared by `parse_event` and `process`.
fn detect_error_envelope(id: &str, val: &Value) -> Option<Error> {
    let err = val.get("error")?;
    // Some gateways (LiteLLM, some vLLM/OpenRouter proxies) attach a
    // top-level `error: null` -- or an empty `error: {}` -- to every
    // normal chunk. Those are not error envelopes; treating them as one
    // would truncate a healthy stream with a spurious 502. Any other
    // shape (populated object, string, number) stays terminal as before.
    if err.is_null() || err.as_object().is_some_and(serde_json::Map::is_empty) {
        tracing::debug!(provider = %id, "skipping null/empty error field in stream chunk");
        return None;
    }
    let status = val
        .pointer("/error/code")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u16::try_from(n).ok())
        .unwrap_or(502);
    let message = val
        .pointer("/error/message")
        .and_then(|v| v.as_str())
        .unwrap_or("upstream error in stream")
        .to_string();
    // Lift the upstream classifier so the mid-stream error surfaces the
    // same `error.type` / `error.code` the non-streaming path carries. The
    // ingress stream-error class reads these to keep the upstream signal
    // instead of collapsing to a generic bucket; the message stays in the
    // error body, matching what the non-stream path surfaces. No new log
    // is emitted here -- the raw frame is never widened onto a log line.
    let (upstream_type, upstream_code) = super::classify_error_value(val);
    Some(Error::upstream_full(
        id,
        status,
        message,
        None,
        upstream_type,
        upstream_code,
    ))
}

/// Lift OpenAI/DeepSeek usage sub-bags into canonical typed slots and
/// deserialize the resulting `usage` object into a `UsageDelta`. Returns
/// `None` when no `usage` key is present (the common non-terminal chunk)
/// or when deserialization fails. Used by `process()`, which does not
/// route through serde's full ChatChunk deserialize and would otherwise
/// drop the terminal usage object entirely.
///
/// Best-effort by contract: a malformed `usage` object yields `None`
/// rather than an error so a bad terminal bag never aborts the stream.
/// To keep that from being a silent differential vs the loud `parse_event`
/// serde path, a present-but-undeserializable `usage` key is logged at
/// debug with the serde error; a missing key stays silent.
fn extract_chunk_usage(provider_id: &str, val: &mut Value) -> Option<UsageDelta> {
    lift_chunk_usage_subbags(val);
    let usage = val.get("usage")?;
    match serde_json::from_value(usage.clone()) {
        Ok(delta) => Some(delta),
        Err(e) => {
            tracing::debug!(
                provider = %provider_id,
                error = %e,
                "dropping malformed streaming usage object"
            );
            None
        }
    }
}

/// JSON-side mirror of `response::lift_and_strip_usage_extras` for the
/// SSE path. Operates on the chunk JSON before serde deserialization
/// because `UsageDelta` has no extras catchall; deserialization would
/// otherwise silently drop the sub-bags. Idempotent: only writes the
/// top-level fields when they are not already populated by the
/// upstream.
fn lift_chunk_usage_subbags(val: &mut Value) {
    // Fast path: non-terminal chunks (the vast majority) carry no `usage`
    // sub-object. Skip the mutable borrow + sub-bag walk entirely for them.
    if val.get("usage").is_none() {
        return;
    }
    let Some(usage) = val.get_mut("usage").and_then(|v| v.as_object_mut()) else {
        return;
    };

    let reasoning_tokens = usage
        .get("completion_tokens_details")
        .and_then(|v| v.get("reasoning_tokens"))
        .and_then(serde_json::Value::as_u64);
    if let Some(n) = reasoning_tokens {
        // Drop a present `null` sentinel before `or_insert`: a top-level
        // `reasoning_tokens: null` from the upstream would otherwise block
        // the sub-bag lift because `entry().or_insert()` treats an occupied
        // entry -- even one holding `null` -- as already set.
        if matches!(usage.get("reasoning_tokens"), Some(Value::Null)) {
            usage.remove("reasoning_tokens");
        }
        usage.entry("reasoning_tokens").or_insert(Value::from(n));
    }

    let cache_read = usage
        .get("prompt_cache_hit_tokens")
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            usage
                .get("prompt_tokens_details")
                .and_then(|v| v.get("cached_tokens"))
                .and_then(serde_json::Value::as_u64)
        });
    if let Some(n) = cache_read {
        // Same null-sentinel guard as `reasoning_tokens` above.
        if matches!(usage.get("cache_read_input_tokens"), Some(Value::Null)) {
            usage.remove("cache_read_input_tokens");
        }
        usage
            .entry("cache_read_input_tokens")
            .or_insert(Value::from(n));
    }

    // Sub-bags themselves are unknown to UsageDelta and would be
    // dropped by serde regardless; an explicit remove makes the
    // intention readable in the trace and keeps any future extras
    // flatten on UsageDelta from accidentally readmitting them.
    for k in [
        "prompt_cache_hit_tokens",
        "prompt_cache_miss_tokens",
        "prompt_tokens_details",
        "completion_tokens_details",
    ] {
        usage.remove(k);
    }
}

/// Walk `choices[].delta` and merge `reasoning_content` into `reasoning`.
/// See `merge_reasoning_keys` in `response.rs` for the rules.
fn coalesce_chunk_reasoning_keys(val: &mut Value) {
    let Some(choices) = val.get_mut("choices").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for choice in choices.iter_mut() {
        if let Some(delta) = choice.get_mut("delta").and_then(|v| v.as_object_mut()) {
            super::response::merge_reasoning_keys(delta);
        }
    }
}

fn apply_chunk_dialect(
    id: &str,
    val: &mut Value,
    dialect: ReasoningDialect,
    reasoning_index: &mut u32,
) -> Result<()> {
    dialect.as_dyn().apply_chunk(id, val, reasoning_index)
}

// ---------------------------------------------------------------------------
// Stateful <think> tag accumulator (used by stream() for RawThinkTag dialect)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
enum ThinkState {
    #[default]
    Outside,
    Inside,
}

/// Cross-chunk state machine for stripping `<think>...</think>` from a
/// streaming content delta. Each call to `process` consumes one SSE data
/// line and returns a `ChatChunk` with the content/reasoning fields separated.
///
/// The accumulator is correct across chunk boundaries that fall INSIDE
/// a tag: e.g. one chunk delivers `"<thi"` and the next delivers
/// `"nk>secret</think>"`. Without buffering, the partial `"<thi"` would
/// be emitted as visible content because `find("<think>")` would not
/// match. The accumulator holds back the longest suffix of the
/// pending output that could be a partial `<think>` (Outside) or
/// `</think>` (Inside) tag, and prepends it on the next call.
pub struct ThinkTagAccumulator {
    state: ThinkState,
    chunk_index: u32,
    /// Bytes held back from the previous chunk because they could be
    /// a partial tag. Bounded by the longer tag length (8 bytes for
    /// `</think>`) so this never grows beyond a handful of chars.
    pending: String,
}

impl Default for ThinkTagAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ThinkTagAccumulator {
    /// Create an empty accumulator positioned at the stream start.
    pub fn new() -> Self {
        Self {
            state: ThinkState::default(),
            chunk_index: 0,
            pending: String::new(),
        }
    }

    /// Drain any held-back `pending` bytes without interpreting them as
    /// tag syntax. Called once at stream end (`[DONE]` or upstream
    /// close): the accumulator holds back a partial-tag suffix waiting
    /// to see if a `<think>` / `</think>` tag completes on the next
    /// chunk, but if the stream terminates first those bytes are real
    /// visible content the client must still receive. Returns `None`
    /// when nothing is buffered (the common case) so the flush is a
    /// no-op on healthy streams. Drains the buffer, so a second call
    /// returns `None`.
    pub fn take_pending(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.pending))
        }
    }

    /// Process one raw SSE data value. Returns `Ok(None)` for `[DONE]` / keepalive.
    pub fn process(&mut self, provider_id: &str, raw: &str) -> Result<Option<ChatChunk>> {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" || trimmed.starts_with(':') {
            return Ok(None);
        }

        let mut val: Value = serde_json::from_str(trimmed)
            .map_err(|e| Error::Streaming(format!("provider `{provider_id}`: SSE JSON: {e}")))?;

        // Mid-stream error envelope: short-circuit before the choices
        // guard so a `{"error":{...}}` frame with no choices returns an
        // upstream error instead of falling through to Ok(None).
        if let Some(err) = detect_error_envelope(provider_id, &val) {
            return Err(err);
        }

        let chunk_id = val
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("chunk")
            .to_string();
        let model = val
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let choices_raw = match val.get("choices").and_then(|v| v.as_array()) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };

        let mut new_choices = Vec::with_capacity(choices_raw.len());
        for choice_val in &choices_raw {
            let index = choice_val
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0) as u32;
            let finish_reason = choice_val
                .get("finish_reason")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let delta_content = choice_val
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let role = choice_val
                .get("delta")
                .and_then(|d| d.get("role"))
                .and_then(|v| v.as_str())
                .and_then(|r| serde_json::from_value(serde_json::Value::String(r.into())).ok());
            let tool_calls = choice_val
                .get("delta")
                .and_then(|d| d.get("tool_calls"))
                .filter(|v| !v.is_null())
                .and_then(|v| v.as_array().cloned());

            let (outside_content, inside_reasoning) = self.split_think_tags(&delta_content);

            let mut delta = ChunkDelta {
                role,
                tool_calls,
                ..Default::default()
            };
            if !outside_content.is_empty() {
                delta.content = Some(outside_content);
            }
            if !inside_reasoning.is_empty() {
                delta.reasoning = Some(inside_reasoning.clone());
                let detail = build_reasoning_detail(
                    &inside_reasoning,
                    ReasoningDialect::RawThinkTag.format_tag(),
                    self.chunk_index,
                );
                delta.reasoning_details.push(detail);
                // saturating_add so a multi-day-running stream
                // (4B+ reasoning chunks) wraps to a no-op rather
                // than silently colliding with index 0 on rollover
                // and breaking downstream consumers that key on
                // index uniqueness within a stream.
                self.chunk_index = self.chunk_index.saturating_add(1);
            }

            new_choices.push(ChunkChoice {
                index,
                delta,
                finish_reason,
                matched_stop_sequence: None,
            });
        }

        // Lift the terminal usage object (sub-bags -> canonical) AFTER
        // the choices loop: `choices_raw` was cloned out of `val`, so
        // no immutable borrow is live and the `&mut val` lift is safe.
        let usage = extract_chunk_usage(provider_id, &mut val);

        Ok(Some(ChatChunk {
            id: chunk_id,
            model,
            choices: new_choices,
            usage,
            opaque_events: Vec::new(),
            upstream_meta: None,
        }))
    }

    /// Split `text` into (outside_think_content, inside_think_content) while
    /// advancing the internal `<think>` / `</think>` state machine.
    ///
    /// Handles three cases:
    ///   1. Tag boundary falls mid-chunk (e.g. only `<think>` arrives with no `</think>`).
    ///   2. Full `<think>...</think>` inside one chunk.
    ///   3. Mixed: text before tag + tag content + text after tag.
    ///
    /// Cross-chunk safety: a partial tag at the END of `text` (e.g.
    /// `"...prefix <thi"`) is held back in `self.pending` and
    /// prepended on the next call so a `</thi` + `nk>secret</think>`
    /// split is still detected and the secret stays in `inside`.
    fn split_think_tags(&mut self, text: &str) -> (String, String) {
        // Prepend any held-back partial tag from the previous chunk.
        let pending = std::mem::take(&mut self.pending);
        let combined: String;
        let combined_ref: &str = if pending.is_empty() {
            text
        } else {
            combined = format!("{pending}{text}");
            &combined
        };

        let mut outside = String::new();
        let mut inside = String::new();
        let mut remaining = combined_ref;

        loop {
            match self.state {
                ThinkState::Outside => {
                    if let Some(pos) = remaining.find("<think>") {
                        outside.push_str(&remaining[..pos]);
                        remaining = &remaining[pos + "<think>".len()..];
                        self.state = ThinkState::Inside;
                    } else {
                        outside.push_str(remaining);
                        break;
                    }
                }
                ThinkState::Inside => {
                    if let Some(pos) = remaining.find("</think>") {
                        inside.push_str(&remaining[..pos]);
                        remaining = &remaining[pos + "</think>".len()..];
                        self.state = ThinkState::Outside;
                    } else {
                        // No closing tag yet -- everything is inside.
                        inside.push_str(remaining);
                        break;
                    }
                }
            }
        }

        // Hold back any trailing partial-tag prefix so a tag straddling
        // a chunk boundary isn't missed. The candidate buffer is whichever
        // string we just appended into based on terminal state.
        match self.state {
            ThinkState::Outside => {
                let cut = longest_partial_tag_suffix(&outside, "<think>");
                if cut > 0 {
                    self.pending = outside[outside.len() - cut..].to_string();
                    outside.truncate(outside.len() - cut);
                }
            }
            ThinkState::Inside => {
                let cut = longest_partial_tag_suffix(&inside, "</think>");
                if cut > 0 {
                    self.pending = inside[inside.len() - cut..].to_string();
                    inside.truncate(inside.len() - cut);
                }
            }
        }

        (outside, inside)
    }
}

/// Return the byte length of the longest suffix of `s` that is a
/// non-empty prefix of `tag`. O(tag.len()^2), bounded by the tag
/// length (max 8 for `</think>`) so cost is constant per call.
fn longest_partial_tag_suffix(s: &str, tag: &str) -> usize {
    let max_len = tag.len().min(s.len());
    for take in (1..=max_len).rev() {
        let start = s.len() - take;
        if !s.is_char_boundary(start) {
            continue;
        }
        let suffix = &s[start..];
        // A full tag match (suffix == tag) means the tag's already
        // been processed by the main loop; we should never reach
        // here with a complete tag, but guard against it just in
        // case to avoid holding back a complete tag forever.
        if suffix.len() < tag.len() && tag.starts_with(suffix) {
            return take;
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::schema::ChunkChoice;
    use serde_json::json;

    // --- Streamed tool-call id synthesis (StreamedToolCallIds) ---

    fn tool_call_chunk(choices: Vec<(u32, Vec<Value>)>) -> ChatChunk {
        ChatChunk {
            id: "chunk-1".into(),
            model: "test".into(),
            choices: choices
                .into_iter()
                .map(|(index, tool_calls)| ChunkChoice {
                    index,
                    delta: ChunkDelta {
                        tool_calls: Some(tool_calls),
                        ..Default::default()
                    },
                    finish_reason: None,
                    matched_stop_sequence: None,
                })
                .collect(),
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: None,
        }
    }

    fn tool_call_id(chunk: &ChatChunk, choice: usize, call: usize) -> Option<String> {
        chunk.choices[choice].delta.tool_calls.as_ref()?[call]
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    /// An id-less indexed streamed tool_call gets a stable synthesized
    /// id on its first delta, and every later delta of the same key
    /// reuses that exact id (so the openai->anthropic pairing keys on
    /// one stable value across the whole tool call).
    #[test]
    fn streamed_tool_call_without_id_gets_synthesized_id_reused_across_deltas() {
        let mut ids = StreamedToolCallIds::default();

        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "type": "function",
                        "function": {"name": "calc", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // A later argument-only delta for the same key carries no id and
        // must be backfilled with the SAME synthesized id.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"arguments": "{\"x\":1}"}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        assert_eq!(tool_call_id(&second, 0, 0).as_deref(), Some("call_0"));
    }

    /// A streamed tool_call whose first delta already carries an id is
    /// untouched; its later id-less argument deltas stay id-less (the
    /// verbatim passthrough shape), so real-id streams are unchanged.
    #[test]
    fn streamed_tool_call_with_upstream_id_is_untouched() {
        let mut ids = StreamedToolCallIds::default();

        let mut first = tool_call_chunk(vec![(
            0,
            vec![
                json!({"index": 0, "id": "call_upstream", "type": "function",
                        "function": {"name": "calc", "arguments": ""}}),
            ],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_upstream"));

        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"arguments": "{}"}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        // Left id-less -- the client already has the id from the first delta.
        assert_eq!(tool_call_id(&second, 0, 0), None);
    }

    /// Two id-less tool_calls at the same tool-call index but different
    /// choices (an `n > 1` response) get DISTINCT synthesized ids so
    /// cross-choice ids never collide.
    #[test]
    fn idless_tool_calls_across_choices_get_distinct_ids() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = tool_call_chunk(vec![
            (
                0,
                vec![json!({"index": 0, "function": {"name": "a", "arguments": ""}})],
            ),
            (
                1,
                vec![json!({"index": 0, "function": {"name": "b", "arguments": ""}})],
            ),
        ]);
        ids.fill_missing_ids("p", &mut chunk).unwrap();
        let id0 = tool_call_id(&chunk, 0, 0);
        let id1 = tool_call_id(&chunk, 1, 0);
        assert_eq!(id0.as_deref(), Some("call_0"));
        assert_eq!(id1.as_deref(), Some("call_1_0"));
        assert_ne!(id0, id1, "cross-choice synthesized ids must be distinct");
    }

    /// The streamed synthesis scheme matches the non-streaming
    /// `normalize_tool_calls` empty-id fallback, so the two paths cannot
    /// drift.
    #[test]
    fn synthesized_streamed_id_matches_non_streaming_scheme() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"name": "calc", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut chunk).unwrap();
        let streamed = tool_call_id(&chunk, 0, 0).expect("synthesized id");

        // Non-streaming path: an empty-id tool_call at array position 0.
        let non_streaming = crate::tool_calls::normalize_tool_calls(
            "p",
            &[json!({"type": "function", "function": {"name": "calc", "arguments": "{}"}})],
        );
        assert_eq!(streamed, non_streaming[0].id);
        assert_eq!(streamed, "call_0");
    }

    /// A chunk with no tool_calls (the common content delta) is left
    /// untouched by the synthesizer.
    #[test]
    fn content_only_chunk_is_untouched_by_synthesizer() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = ChatChunk {
            id: "c".into(),
            model: "m".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("hi".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
            upstream_meta: None,
        };
        ids.fill_missing_ids("p", &mut chunk).unwrap();
        assert!(chunk.choices[0].delta.tool_calls.is_none());
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
    }

    /// A real upstream id seen FIRST for one slot must not later be
    /// re-minted onto a different id-less slot: when a subsequent id-less
    /// slot's scheme id would equal that reserved real id, the synthesizer
    /// picks a unique alternative instead of colliding.
    #[test]
    fn real_id_reserved_before_a_colliding_synthesized_id_is_minted() {
        let mut ids = StreamedToolCallIds::default();

        // Slot (0,1) carries the real upstream id "call_0" first.
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 1, "id": "call_0", "type": "function",
                        "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // Slot (0,0) is id-less: its scheme id would be "call_0", already
        // reserved by (0,1). It must mint a distinct id, not collide.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"name": "b", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        let minted = tool_call_id(&second, 0, 0).expect("id-less slot minted an id");
        assert_ne!(
            minted, "call_0",
            "mint must not collide with a reserved real id"
        );
    }

    /// A slot whose first delta is id-less mints an id the client sees; a
    /// LATER delta for the SAME slot that carries a real upstream id must
    /// NOT replace the minted id -- the call keeps emitting one stable id
    /// across all its deltas.
    #[test]
    fn late_real_id_for_a_minted_slot_keeps_the_established_id() {
        let mut ids = StreamedToolCallIds::default();

        // First delta id-less: mints "call_0".
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "type": "function",
                        "function": {"name": "calc", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // Later delta for the same slot now carries a real id; it must be
        // overwritten back to the established minted id.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "id": "call_real_late",
                        "function": {"arguments": "{}"}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        assert_eq!(tool_call_id(&second, 0, 0).as_deref(), Some("call_0"));
    }

    /// A real upstream id that collides with an id ALREADY minted for a
    /// different slot is genuinely ambiguous (two calls would pair on one
    /// id): the stream fails hard rather than mispairing a tool_result.
    #[test]
    fn real_id_colliding_with_a_prior_synthesized_id_fails_the_stream() {
        let mut ids = StreamedToolCallIds::default();

        // Slot (0,0) is id-less: mints "call_0".
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // A different slot (0,1) now carries the real id "call_0".
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 1, "id": "call_0", "type": "function",
                        "function": {"name": "b", "arguments": ""}})],
        )]);
        let err = ids
            .fill_missing_ids("p", &mut second)
            .expect_err("ambiguous id collision must fail the stream");
        match err {
            Error::Streaming(msg) => assert!(
                msg.contains("collides"),
                "error must name the collision, got: {msg}"
            ),
            other => panic!("expected Error::Streaming, got: {other:?}"),
        }
    }

    /// A slot whose first delta carried a real upstream id keeps that id
    /// stable: a LATER delta for the SAME slot that carries a DIFFERENT
    /// real id must be overwritten back to the established id, so the call
    /// never emits two different ids across its deltas.
    #[test]
    fn real_id_slot_keeps_its_id_against_a_late_differing_real_id() {
        let mut ids = StreamedToolCallIds::default();

        // First delta establishes the real id "call_first" for slot (0,0).
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "id": "call_first", "type": "function",
                        "function": {"name": "calc", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_first"));

        // A later delta for the same slot carries a different real id; it
        // must be overwritten back to the established id.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "id": "call_changed",
                        "function": {"arguments": "{}"}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        assert_eq!(tool_call_id(&second, 0, 0).as_deref(), Some("call_first"));
    }

    /// Two DIFFERENT slots that both carry the same real upstream id are
    /// genuinely ambiguous (a tool_result would pair on one id shared by
    /// two calls): the stream fails hard rather than mispairing.
    #[test]
    fn real_id_colliding_with_a_prior_real_id_on_a_different_slot_fails_the_stream() {
        let mut ids = StreamedToolCallIds::default();

        // Slot (0,0) carries the real id "call_dup".
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "id": "call_dup", "type": "function",
                        "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_dup"));

        // A different slot (0,1) carries the SAME real id "call_dup".
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 1, "id": "call_dup", "type": "function",
                        "function": {"name": "b", "arguments": ""}})],
        )]);
        let err = ids
            .fill_missing_ids("p", &mut second)
            .expect_err("duplicate real id on a different slot must fail the stream");
        match err {
            Error::Streaming(msg) => assert!(
                msg.contains("collides"),
                "error must name the collision, got: {msg}"
            ),
            other => panic!("expected Error::Streaming, got: {other:?}"),
        }
    }

    /// A real upstream id whose `index` is malformed cannot key a slot, but
    /// it still reaches the wire and must be reserved by value: a later
    /// id-less slot whose scheme id would equal that reserved real id mints
    /// a distinct id instead of colliding.
    #[test]
    fn real_id_with_malformed_index_is_reserved_against_a_later_mint() {
        let mut ids = StreamedToolCallIds::default();

        // A real id "call_0" arrives with a missing index -> no slot key,
        // reserved by value, passed through verbatim.
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"id": "call_0", "type": "function",
                        "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // Slot (0,0) is id-less: its scheme id "call_0" is already reserved
        // by the malformed real id, so it must mint a distinct id.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"name": "b", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut second).unwrap();
        let minted = tool_call_id(&second, 0, 0).expect("id-less slot minted an id");
        assert_ne!(
            minted, "call_0",
            "mint must not collide with a reserved malformed-index real id"
        );
    }

    /// A real upstream id whose `index` is malformed still fails the stream
    /// when it duplicates an id a keyed slot already owns (mispairing risk).
    #[test]
    fn real_id_with_malformed_index_colliding_with_a_keyed_slot_fails_the_stream() {
        let mut ids = StreamedToolCallIds::default();

        // Slot (0,0) mints "call_0".
        let mut first = tool_call_chunk(vec![(
            0,
            vec![json!({"index": 0, "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut first).unwrap();
        assert_eq!(tool_call_id(&first, 0, 0).as_deref(), Some("call_0"));

        // A malformed-index real id duplicating that minted id must fail.
        let mut second = tool_call_chunk(vec![(
            0,
            vec![json!({"id": "call_0", "type": "function",
                        "function": {"name": "b", "arguments": ""}})],
        )]);
        let err = ids
            .fill_missing_ids("p", &mut second)
            .expect_err("malformed real id duplicating a keyed slot must fail");
        assert!(matches!(err, Error::Streaming(_)));
    }

    /// An id-less streamed tool_call with a MISSING `index` cannot form a
    /// slot key; it must fail the stream rather than collapse to slot 0.
    #[test]
    fn idless_tool_call_missing_index_fails_the_stream() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = tool_call_chunk(vec![(
            0,
            vec![json!({"type": "function", "function": {"name": "a", "arguments": ""}})],
        )]);
        let err = ids
            .fill_missing_ids("p", &mut chunk)
            .expect_err("missing index must fail the stream");
        assert!(matches!(err, Error::Streaming(_)));
    }

    /// An id-less streamed tool_call with a non-u32 `index` (negative,
    /// fractional, or out-of-range) must fail the stream, not fall back to 0.
    #[test]
    fn idless_tool_call_non_u32_index_fails_the_stream() {
        for bad_index in [
            json!(-1),
            json!(1.5),
            json!("0"),
            json!(u64::from(u32::MAX) + 1),
        ] {
            let mut ids = StreamedToolCallIds::default();
            let mut chunk = tool_call_chunk(vec![(
                0,
                vec![json!({"index": bad_index,
                            "function": {"name": "a", "arguments": ""}})],
            )]);
            let err = ids
                .fill_missing_ids("p", &mut chunk)
                .expect_err("non-u32 index must fail the stream");
            assert!(matches!(err, Error::Streaming(_)), "index {bad_index:?}");
        }
    }

    /// Two distinct id-less tool_calls with valid distinct indices get
    /// distinct synthesized ids (no collapse onto slot 0).
    #[test]
    fn two_idless_tool_calls_with_distinct_indices_get_distinct_ids() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = tool_call_chunk(vec![(
            0,
            vec![
                json!({"index": 0, "function": {"name": "a", "arguments": ""}}),
                json!({"index": 1, "function": {"name": "b", "arguments": ""}}),
            ],
        )]);
        ids.fill_missing_ids("p", &mut chunk).unwrap();
        let id0 = tool_call_id(&chunk, 0, 0);
        let id1 = tool_call_id(&chunk, 0, 1);
        assert_eq!(id0.as_deref(), Some("call_0"));
        assert_eq!(id1.as_deref(), Some("call_1"));
        assert_ne!(id0, id1, "distinct indices must yield distinct ids");
    }

    /// A tool_call that already carries an id is untouched regardless of a
    /// missing / malformed `index` -- the index guard applies only to
    /// id-less calls.
    #[test]
    fn tool_call_with_id_and_bad_index_is_untouched() {
        let mut ids = StreamedToolCallIds::default();
        let mut chunk = tool_call_chunk(vec![(
            0,
            vec![json!({"id": "call_kept", "type": "function",
                        "function": {"name": "a", "arguments": ""}})],
        )]);
        ids.fill_missing_ids("p", &mut chunk).unwrap();
        assert_eq!(tool_call_id(&chunk, 0, 0).as_deref(), Some("call_kept"));
    }

    fn delta_chunk(content: Option<&str>, reasoning_content: Option<&str>) -> String {
        let mut delta = json!({});
        if let Some(c) = content {
            delta["content"] = json!(c);
        }
        if let Some(r) = reasoning_content {
            delta["reasoning_content"] = json!(r);
        }
        let chunk = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": null
            }]
        });
        chunk.to_string()
    }

    #[test]
    fn done_returns_none() {
        assert!(
            parse_event("t", "[DONE]", ReasoningDialect::OpenAi, &mut 0)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_event("t", "  [DONE]  ", ReasoningDialect::OpenAi, &mut 0)
                .unwrap()
                .is_none()
        );
        assert!(
            parse_event("t", "", ReasoningDialect::OpenAi, &mut 0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn openai_basic_delta() {
        let raw = delta_chunk(Some("hello"), None);
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
        assert!(chunk.choices[0].delta.reasoning_details.is_empty());
    }

    #[test]
    fn deepseek_lifts_reasoning_content_in_delta() {
        let raw = delta_chunk(Some("answer"), Some("chain of thought"));
        let chunk = parse_event("t", &raw, ReasoningDialect::DeepSeek, &mut 0)
            .unwrap()
            .unwrap();
        let details = &chunk.choices[0].delta.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("deepseek-v1"));
        assert_eq!(details[0].payload["text"], "chain of thought");
        // Original content preserved
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("answer"));
    }

    #[test]
    fn vllm_lifts_reasoning_content_in_delta() {
        let raw = delta_chunk(None, Some("vllm trace"));
        let chunk = parse_event("t", &raw, ReasoningDialect::Vllm, &mut 0)
            .unwrap()
            .unwrap();
        let details = &chunk.choices[0].delta.reasoning_details;
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].format.as_deref(), Some("vllm-reasoning-v1"));
    }

    /// Pin the streaming reasoning-index contract: successive DeepSeek
    /// reasoning chunks driven through `parse_event` with a single shared
    /// per-stream counter (exactly as `stream()` threads it) must carry
    /// `index` 0, 1, 2, ... -- NOT all 0. Before the fix the lifter
    /// hardcoded index 0, collapsing every streamed delta onto one block.
    #[test]
    fn deepseek_streaming_reasoning_detail_index_increments() {
        let mut reasoning_index: u32 = 0;
        for expected in [0u32, 1, 2] {
            let raw = delta_chunk(Some("answer"), Some("step"));
            let chunk = parse_event("t", &raw, ReasoningDialect::DeepSeek, &mut reasoning_index)
                .unwrap()
                .unwrap();
            let details = &chunk.choices[0].delta.reasoning_details;
            assert_eq!(details.len(), 1);
            assert_eq!(
                details[0].index,
                Some(expected),
                "streamed reasoning detail index must increment per chunk"
            );
        }
    }

    /// A chunk with no reasoning content must NOT advance the counter, so
    /// the index stays aligned to actual reasoning deltas (0, then 1 after
    /// the gap -- not 0 then 2).
    #[test]
    fn vllm_streaming_reasoning_index_skips_non_reasoning_chunks() {
        let mut reasoning_index: u32 = 0;

        let c0 = parse_event(
            "t",
            &delta_chunk(None, Some("first")),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c0.choices[0].delta.reasoning_details[0].index, Some(0));

        // Plain content chunk: no reasoning -> counter unchanged.
        let c1 = parse_event(
            "t",
            &delta_chunk(Some("visible"), None),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert!(c1.choices[0].delta.reasoning_details.is_empty());

        let c2 = parse_event(
            "t",
            &delta_chunk(None, Some("second")),
            ReasoningDialect::Vllm,
            &mut reasoning_index,
        )
        .unwrap()
        .unwrap();
        assert_eq!(c2.choices[0].delta.reasoning_details[0].index, Some(1));
    }

    // --- ThinkTagAccumulator tests ---

    #[test]
    fn think_tag_whole_in_one_chunk() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = delta_chunk(Some("<think>reasoning</think>answer"), None);
        let chunk = acc.process("t", &raw).unwrap().unwrap();
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("answer"));
        assert_eq!(
            chunk.choices[0].delta.reasoning.as_deref(),
            Some("reasoning")
        );
    }

    #[test]
    fn think_tag_open_in_first_chunk_close_in_second() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: opens but does not close the tag
        let raw1 = delta_chunk(Some("<think>partial reason"), None);
        let c1 = acc.process("t", &raw1).unwrap().unwrap();
        assert!(c1.choices[0].delta.content.is_none());
        assert_eq!(
            c1.choices[0].delta.reasoning.as_deref(),
            Some("partial reason")
        );

        // Chunk 2: closes the tag, continues with normal content
        let raw2 = delta_chunk(Some("ing continued</think>normal text"), None);
        let c2 = acc.process("t", &raw2).unwrap().unwrap();
        assert_eq!(
            c2.choices[0].delta.reasoning.as_deref(),
            Some("ing continued")
        );
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("normal text"));
    }

    #[test]
    fn think_tag_no_tag_passthrough() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = delta_chunk(Some("no tags here"), None);
        let chunk = acc.process("t", &raw).unwrap().unwrap();
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("no tags here")
        );
        assert!(chunk.choices[0].delta.reasoning.is_none());
    }

    #[test]
    fn think_tag_split_across_three_chunks() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: before tag
        let c1 = acc
            .process("t", &delta_chunk(Some("before<think>"), None))
            .unwrap()
            .unwrap();
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("before"));
        assert!(c1.choices[0].delta.reasoning.is_none());

        // Chunk 2: inside tag, no close
        let c2 = acc
            .process("t", &delta_chunk(Some("thinking..."), None))
            .unwrap()
            .unwrap();
        assert!(c2.choices[0].delta.content.is_none());
        assert_eq!(
            c2.choices[0].delta.reasoning.as_deref(),
            Some("thinking...")
        );

        // Chunk 3: close tag + after
        let c3 = acc
            .process("t", &delta_chunk(Some("</think>after"), None))
            .unwrap()
            .unwrap();
        // Empty reasoning (closing tag with no content before it)
        assert!(c3.choices[0].delta.reasoning.is_none());
        assert_eq!(c3.choices[0].delta.content.as_deref(), Some("after"));
    }

    /// Pin: open `<think>` tag SPLIT across chunk boundaries.
    /// Without buffering, the partial `<thi` in chunk 1 emits as
    /// visible content; chunk 2 (`nk>secret</think>`) also fails to
    /// match `<think>` and goes to visible content, LEAKING the
    /// reasoning text. The accumulator buffers the partial-tag suffix
    /// and prepends it on the next call.
    #[test]
    fn think_open_tag_split_across_chunks_does_not_leak() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: only the first 4 bytes of `<think>` arrive.
        let c1 = acc
            .process("t", &delta_chunk(Some("<thi"), None))
            .unwrap()
            .unwrap();
        // The partial `<thi` is held back -- nothing visible yet.
        assert!(c1.choices[0].delta.content.is_none());
        assert!(c1.choices[0].delta.reasoning.is_none());

        // Chunk 2: rest of the open tag + secret + close + visible.
        let c2 = acc
            .process("t", &delta_chunk(Some("nk>secret</think>visible"), None))
            .unwrap()
            .unwrap();
        // `secret` MUST be in reasoning, NOT visible content.
        assert_eq!(c2.choices[0].delta.reasoning.as_deref(), Some("secret"));
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("visible"));
    }

    /// Pin: close `</think>` tag SPLIT across chunk boundaries. The
    /// reasoning text up to the partial close must not flow back as
    /// visible content; the close tag must be detected on the next
    /// chunk.
    #[test]
    fn think_close_tag_split_across_chunks() {
        let mut acc = ThinkTagAccumulator::new();

        // Chunk 1: open tag + reasoning + first few bytes of close tag.
        let c1 = acc
            .process("t", &delta_chunk(Some("<think>reason</thi"), None))
            .unwrap()
            .unwrap();
        // `reason` is reasoning. The `</thi` is held back, so reasoning
        // emitted on this chunk is just `reason` (no extra suffix).
        assert_eq!(c1.choices[0].delta.reasoning.as_deref(), Some("reason"));
        assert!(c1.choices[0].delta.content.is_none());

        // Chunk 2: rest of close tag + visible.
        let c2 = acc
            .process("t", &delta_chunk(Some("nk>after"), None))
            .unwrap()
            .unwrap();
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("after"));
        assert!(c2.choices[0].delta.reasoning.is_none());
    }

    /// Pin: a one-character chunk that happens to be `<` does not
    /// flood `pending` with arbitrary content -- the buffer is
    /// bounded by the tag length.
    #[test]
    fn think_tag_lone_lt_holds_back_at_most_tag_length() {
        let mut acc = ThinkTagAccumulator::new();
        let _ = acc
            .process("t", &delta_chunk(Some("<"), None))
            .unwrap()
            .unwrap();
        // Now follow with content that is NOT a think tag.
        let c = acc
            .process("t", &delta_chunk(Some("not a think tag"), None))
            .unwrap()
            .unwrap();
        // The held-back `<` flows through with the next chunk.
        assert_eq!(
            c.choices[0].delta.content.as_deref(),
            Some("<not a think tag")
        );
    }

    /// Pin: innocent content that ends with a partial-tag-looking
    /// suffix gets delayed by exactly one chunk (no leak, no loss).
    #[test]
    fn think_tag_innocent_partial_suffix_delayed_one_chunk() {
        let mut acc = ThinkTagAccumulator::new();
        let c1 = acc
            .process("t", &delta_chunk(Some("say <th"), None))
            .unwrap()
            .unwrap();
        // `<th` looks like a partial open tag; held back. `say ` flows.
        assert_eq!(c1.choices[0].delta.content.as_deref(), Some("say "));

        let c2 = acc
            .process("t", &delta_chunk(Some("anks!"), None))
            .unwrap()
            .unwrap();
        // Combined `<thanks!` is not a tag; whole thing flows visibly.
        assert_eq!(c2.choices[0].delta.content.as_deref(), Some("<thanks!"));
    }

    /// Pin: held-back pending bytes that are a prefix of `<think>` but
    /// never complete must be recoverable at stream end via
    /// `take_pending`. Without a flush, those bytes (real visible
    /// content) are silently dropped when the stream terminates while
    /// still holding a partial-tag suffix.
    #[test]
    fn take_pending_flushes_held_back_partial_open_tag() {
        let mut acc = ThinkTagAccumulator::new();

        // A chunk ending in `<thi` holds the partial tag back: nothing
        // visible is emitted on this chunk.
        let c = acc
            .process("t", &delta_chunk(Some("hello<thi"), None))
            .unwrap()
            .unwrap();
        assert_eq!(c.choices[0].delta.content.as_deref(), Some("hello"));

        // Stream ends here (no further chunk to complete the tag).
        // take_pending must surface the held-back `<thi`.
        assert_eq!(acc.take_pending().as_deref(), Some("<thi"));
        // Draining is idempotent: a second call returns None.
        assert!(acc.take_pending().is_none());
    }

    /// `take_pending` returns None when nothing is buffered, so the
    /// stream-end flush is a no-op on the common case.
    #[test]
    fn take_pending_is_none_when_buffer_empty() {
        let mut acc = ThinkTagAccumulator::new();
        let _ = acc
            .process("t", &delta_chunk(Some("plain visible text"), None))
            .unwrap()
            .unwrap();
        assert!(acc.take_pending().is_none());
    }

    // --- Terminal-chunk usage sub-bag lift tests ---

    fn terminal_chunk_with_usage(usage: serde_json::Value) -> String {
        json!({
            "id": "chunk-final",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": usage
        })
        .to_string()
    }

    /// A non-terminal chunk carries no `usage` object. The usage-subbag
    /// lift must leave it byte-for-byte untouched (the early-out fires
    /// before any mutable borrow).
    #[test]
    fn non_terminal_chunk_without_usage_is_untouched() {
        let mut val = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{"index": 0, "delta": {"content": "hi"}}]
        });
        let before = val.clone();
        lift_chunk_usage_subbags(&mut val);
        assert_eq!(val, before, "non-terminal chunk must be untouched");
    }

    /// OpenAI terminal chunk delivers `completion_tokens_details
    /// .reasoning_tokens` in the final `usage` object. `UsageDelta`
    /// has no extras catchall; the sub-bag must be lifted into the
    /// canonical typed `reasoning_tokens` field BEFORE serde would
    /// otherwise drop it.
    #[test]
    fn terminal_chunk_lifts_reasoning_tokens() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "completion_tokens_details": {"reasoning_tokens": 7}
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    /// DeepSeek terminal chunk delivers `prompt_cache_hit_tokens`
    /// directly on `usage`. Lift into canonical `cache_read_input_tokens`.
    #[test]
    fn terminal_chunk_lifts_cache_read_from_deepseek() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::DeepSeek, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    /// OpenAI terminal chunk delivers `prompt_tokens_details
    /// .cached_tokens`. Lift into canonical `cache_read_input_tokens`.
    #[test]
    fn terminal_chunk_lifts_cache_read_from_openai_prompt_tokens_details() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        let usage = chunk.usage.expect("terminal usage present");
        assert_eq!(usage.cache_read_input_tokens, Some(64));
    }

    /// Idempotency mirror of the response-side test: when the
    /// upstream already set a top-level canonical field, the lift
    /// from the sub-bag does NOT overwrite it.
    #[test]
    fn terminal_chunk_lift_does_not_clobber_already_set_field() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "reasoning_tokens": 11,
            "completion_tokens_details": {"reasoning_tokens": 99}
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(chunk.usage.unwrap().reasoning_tokens, Some(11));
    }

    /// Regression: when the upstream emits `reasoning_tokens: null` at
    /// the top level (a present-but-null sentinel) the sub-bag lift from
    /// `completion_tokens_details.reasoning_tokens` must still land.
    /// Before the fix, `entry().or_insert()` treated the null-occupied
    /// entry as already set and silently dropped the sub-bag value.
    #[test]
    fn terminal_chunk_null_sentinel_does_not_block_subbag_lift() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 10,
            "completion_tokens": 20,
            "total_tokens": 30,
            "reasoning_tokens": null,
            "completion_tokens_details": {"reasoning_tokens": 5}
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.usage.unwrap().reasoning_tokens,
            Some(5),
            "null sentinel must be replaced by sub-bag value"
        );
    }

    /// Regression: when the upstream emits `cache_read_input_tokens: null`
    /// at the top level the sub-bag lift must still set the field.
    #[test]
    fn terminal_chunk_null_cache_sentinel_does_not_block_subbag_lift() {
        let raw = terminal_chunk_with_usage(json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "cache_read_input_tokens": null,
            "prompt_tokens_details": {"cached_tokens": 64}
        }));
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .unwrap()
            .unwrap();
        assert_eq!(
            chunk.usage.unwrap().cache_read_input_tokens,
            Some(64),
            "null cache sentinel must be replaced by sub-bag value"
        );
    }

    /// A mid-stream JSON error envelope (Azure content filter, OpenRouter
    /// overload, NIM auth-wall) must produce `Error::Upstream`, not
    /// `Error::NormalizeResponse("chunk deserialize: ...")`.
    #[test]
    fn mid_stream_error_envelope_returns_upstream_error() {
        let raw = json!({
            "error": {
                "message": "content management policy violation",
                "code": 403
            }
        })
        .to_string();
        let err = parse_event("test-provider", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream {
                provider,
                status,
                body,
                ..
            } => {
                assert_eq!(provider, "test-provider");
                assert_eq!(status, 403);
                assert!(
                    body.contains("content management policy violation"),
                    "error body must contain upstream message, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// When the error envelope has no numeric `code`, status defaults to 502.
    #[test]
    fn mid_stream_error_envelope_non_numeric_code_defaults_to_502() {
        let raw = json!({
            "error": {
                "message": "service overloaded",
                "code": "rate_limit_exceeded"
            }
        })
        .to_string();
        let err = parse_event("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 502),
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// An out-of-range numeric `code` (> u16::MAX) must NOT wrap silently
    /// to a nonsense status -- it falls through to the 502 default. Before
    /// the `u16::try_from` guard, `as u16` wrapped 70000 to 4464.
    #[test]
    fn mid_stream_error_envelope_out_of_range_code_defaults_to_502() {
        let raw = json!({
            "error": {
                "message": "overflowing code",
                "code": 70000
            }
        })
        .to_string();
        let err = parse_event("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(
                status, 502,
                "out-of-range code must default to 502, not wrap"
            ),
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A terminal chunk driven through `ThinkTagAccumulator::process`
    /// must carry its `usage` token counts on the emitted `ChatChunk`.
    /// Before the fix `process()` hardcoded `usage: None`, dropping the
    /// terminal usage object (and its lifted sub-bag) entirely on the
    /// RawThinkTag streaming path.
    #[test]
    fn process_terminal_chunk_carries_usage_with_subbag_lift() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = json!({
            "id": "chunk-final",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30,
                "completion_tokens_details": {"reasoning_tokens": 7}
            }
        })
        .to_string();
        let chunk = acc.process("t", &raw).unwrap().unwrap();
        let usage = chunk.usage.expect("terminal usage must reach the chunk");
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(30));
        // Sub-bag lift lands the reasoning tokens on the canonical slot.
        assert_eq!(usage.reasoning_tokens, Some(7));
    }

    /// A mid-stream error envelope with NO `choices` key must produce
    /// `Error::Upstream` from `process()` (the RawThinkTag path), not fall
    /// through to `Ok(None)` via the missing-choices guard.
    #[test]
    fn process_mid_stream_error_envelope_returns_upstream_error() {
        let mut acc = ThinkTagAccumulator::new();
        let raw = json!({
            "error": {
                "message": "forbidden by content policy",
                "code": 403
            }
        })
        .to_string();
        let err = acc.process("test-provider", &raw).unwrap_err();
        match err {
            Error::Upstream {
                provider,
                status,
                body,
                ..
            } => {
                assert_eq!(provider, "test-provider");
                assert_eq!(status, 403);
                assert!(
                    body.contains("forbidden by content policy"),
                    "error body must carry the upstream message, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A mid-stream error frame carrying an `error.type` / `error.code`
    /// classifier must lift both onto the surfaced `Error::Upstream` (and
    /// keep the message in the body), matching what the non-streaming path
    /// carries. Before the fix the mid-frame error dropped type/code, so
    /// the ingress stream-error class collapsed to a generic bucket.
    #[test]
    fn mid_stream_error_envelope_lifts_type_and_code() {
        let raw = json!({
            "error": {
                "type": "rate_limit_exceeded",
                "code": "slow_down",
                "message": "too many requests"
            }
        })
        .to_string();
        let err = parse_event("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream {
                status,
                upstream_type,
                upstream_code,
                body,
                ..
            } => {
                // Non-numeric code -> 502 default; type/code lifted.
                assert_eq!(status, 502);
                assert_eq!(upstream_type.as_deref(), Some("rate_limit_exceeded"));
                assert_eq!(upstream_code.as_deref(), Some("slow_down"));
                assert!(
                    body.contains("too many requests"),
                    "the upstream message text must reach the error body, got: {body}"
                );
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A normal content chunk carrying a top-level `error: null` (emitted
    /// on every chunk by LiteLLM and some vLLM/OpenRouter proxies) must
    /// pass through as ordinary content, NOT be misclassified as a 502
    /// that truncates the healthy stream.
    #[test]
    fn null_error_field_passes_through_as_content() {
        let raw = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {"content": "hello"},
                "finish_reason": null
            }],
            "error": null
        })
        .to_string();
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .expect("null error must not abort the stream")
            .expect("null error chunk must yield content");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hello"));
    }

    /// An empty `error: {}` object is likewise not a real error envelope
    /// and must pass through as normal content.
    #[test]
    fn empty_error_object_passes_through_as_content() {
        let raw = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {"content": "world"},
                "finish_reason": null
            }],
            "error": {}
        })
        .to_string();
        let chunk = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0)
            .expect("empty error object must not abort the stream")
            .expect("empty error chunk must yield content");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("world"));
    }

    /// A populated error object still surfaces as `Error::Upstream` with
    /// the mapped status -- the loosening must not let a real structured
    /// error through (the router's pre-first-chunk fallback depends on it).
    #[test]
    fn populated_error_object_stays_terminal() {
        let raw = json!({
            "error": {"code": 429, "message": "rate limited"}
        })
        .to_string();
        let err = parse_event("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 429);
                assert!(body.contains("rate limited"), "got: {body}");
            }
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// A non-object error (string here) is not null and not an empty
    /// object, so it stays terminal exactly as before -- defaulting to a
    /// 502 with the generic message.
    #[test]
    fn string_error_field_stays_terminal() {
        let raw = json!({"error": "something went wrong"}).to_string();
        let err = parse_event("p", &raw, ReasoningDialect::OpenAi, &mut 0).unwrap_err();
        match err {
            Error::Upstream { status, .. } => assert_eq!(status, 502),
            other => panic!("expected Error::Upstream, got: {other:?}"),
        }
    }

    /// The null/empty skip emits exactly one DEBUG event carrying no
    /// payload bytes, so a new gateway shape stays visible to operators
    /// without leaking chunk content.
    #[test]
    fn null_error_skip_emits_one_clean_debug_log() {
        let raw = json!({
            "id": "chunk-1",
            "model": "test",
            "choices": [{
                "index": 0,
                "delta": {"content": "secret token text"},
                "finish_reason": null
            }],
            "error": null
        })
        .to_string();
        let events = routectl_testkit::capture_events(|| {
            let _ = parse_event("t", &raw, ReasoningDialect::OpenAi, &mut 0);
        });
        let debug: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::DEBUG)
            .filter(|e| e.message.contains("error field"))
            .collect();
        assert_eq!(
            debug.len(),
            1,
            "expected exactly one skip debug log, got: {events:?}"
        );
        for e in &events {
            assert!(
                !e.message.contains("secret token text"),
                "skip log must not carry payload bytes: {e:?}"
            );
            for (_, v) in &e.fields {
                assert!(
                    !v.contains("secret token text"),
                    "skip log field must not carry payload bytes: {e:?}"
                );
            }
        }
    }
}
