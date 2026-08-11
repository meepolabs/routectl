//! Pure, stateless positive-evidence detectors over a terminal successful
//! non-streaming [`ChatResponse`].
//!
//! Each detector reads STRUCTURAL evidence from the canonical response --
//! never "no error occurred" -- and emits a [`CapabilityObservation`]. A
//! shared clean-stop gate runs FIRST and fails closed: unless the response
//! is an unambiguous clean success (exactly one choice, no refusal, a
//! finish_reason in the explicit allow set), every detector is skipped and
//! `detect` returns an empty vector. New or degraded stop vocabulary can
//! never misclassify.
//!
//! The module carries no clock, no I/O, and no config or registry access:
//! `now` and admission belong to stage two (the caller), so this stage is
//! never replayed. The two-stage contract is described on
//! [`crate::router::capability_observe`].
//!
//! Detectors emit only well-known capability keys and the pinned
//! evidence-class tokens from `routectl_core::capability`; token literals
//! are never re-declared here.

use routectl_core::capability::{
    CACHE_HIT, PROMPT_CACHING, SCHEMA_MISMATCH, SCHEMA_PARSE, SEARCH_ABSENT_FORCED, SEARCH_BLOCKS,
    STRUCTURED_OUTPUT, SignalTier, THINKING, THINKING_BLOCKS, WEB_SEARCH,
};
use routectl_core::{
    ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent, Usage,
};
use serde_json::Value;

/// Finish-reason tokens that count as a clean stop. These are the
/// CANONICAL post-normalization tokens: the Anthropic-shape egress
/// collapses `end_turn`/`stop_sequence` to `"stop"` and `tool_use` to
/// `"tool_calls"`, while `pause_turn` passes through verbatim. The set is
/// closed and fail-closed: `"length"`, `"content_filter"`, an absent
/// finish_reason, and any unknown token all reject.
const CLEAN_STOP_ALLOW: &[&str] = &["stop", "tool_calls", "pause_turn"];

/// Content-block `type` discriminants that constitute web-search evidence
/// when they appear as a forward-compat [`ContentPart::Other`] block (no
/// typed search block exists in the canonical schema).
///
/// Deliberately narrow: `web_search_tool_result` is the block that carries
/// actual search results, so its presence is unambiguous proof a search
/// ran. The generic `server_tool_use` block is NOT in this set -- it is
/// shared with code execution and other server tools, so keying on its
/// bare `type` would over-attribute.
const SEARCH_EVIDENCE_TYPE_TAGS: &[&str] = &["web_search_tool_result"];

/// Usage `server_tool_use` sub-keys whose positive integer count proves a
/// web search ran. The `server_tool_use` object is a per-server-tool
/// counter map (Anthropic reports `web_search_requests` there), so the
/// detector keys ONLY on the search-specific counter -- summing every
/// integer would credit web search for an unrelated server tool such as
/// code execution.
const WEB_SEARCH_REQUEST_KEYS: &[&str] = &["web_search_requests"];

/// Per-capability request intent, derived router-side while the request is
/// still in hand. Detectors read intent from here; they never consult the
/// config, the registry, or a clock.
#[derive(Debug, Clone, Default)]
pub struct DetectorContext {
    /// A structured-output format (or strict tool) was requested.
    pub strict_output_requested: bool,
    /// Top-level required property names of the requested output schema.
    /// Empty when no schema keys were declared (the shape check then
    /// passes on any parseable body).
    pub requested_schema_required_keys: Vec<String>,
    /// A web-search tool was forced via `tool_choice` (not merely offered
    /// as `auto`). Only a forced request produces a suspected-absence
    /// observation.
    pub forced_web_search: bool,
    /// Extended thinking / reasoning was requested.
    pub reasoning_requested: bool,
    /// Prompt caching was requested (cache breakpoints present).
    pub cache_requested: bool,
}

/// Which side of a capability an observation attests to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationDirection {
    /// Structural proof the capability worked on this response.
    Verified,
    /// The capability was requested (or forced) but the expected evidence
    /// was absent on an otherwise clean response.
    SuspectAbsence,
}

/// A single positive-detection observation. Carries only what stage-two
/// admission may consult; `source` and `ts` are added at stage-two
/// admission, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityObservation {
    /// Canonical capability key (a well-known key const).
    pub capability_key: &'static str,
    /// Pinned evidence-class token attributing the observation.
    pub evidence_class: &'static str,
    /// Whether the capability was verified working or suspected absent.
    pub direction: ObservationDirection,
    /// Confidence tier: `SelfIdentifying` for structural proof,
    /// `Inferred` for a suspected absence that needs corroboration.
    pub tier: SignalTier,
}

/// Run the clean-stop gate, then every detector, over a terminal
/// successful non-streaming response. Returns an empty vector when the
/// gate rejects; otherwise one observation per detector that fired.
pub fn detect(ctx: &DetectorContext, resp: &ChatResponse) -> Vec<CapabilityObservation> {
    let Some(choice) = clean_stop_choice(resp) else {
        return Vec::new();
    };

    let mut observations = Vec::new();
    if let Some(obs) = detect_structured_output(ctx, choice) {
        observations.push(obs);
    }
    if let Some(obs) = detect_web_search(ctx, resp.usage.as_ref(), &choice.message) {
        observations.push(obs);
    }
    if let Some(obs) = detect_prompt_caching(resp.usage.as_ref()) {
        observations.push(obs);
    }
    if let Some(obs) = detect_thinking(ctx, resp.usage.as_ref(), &choice.message) {
        observations.push(obs);
    }
    observations
}

/// The single clean-stop choice, or `None` when the gate rejects. Requires
/// exactly one choice, a finish_reason in the allow set, and no refusal.
fn clean_stop_choice(resp: &ChatResponse) -> Option<&Choice> {
    let [choice] = resp.choices.as_slice() else {
        return None;
    };
    let reason = choice.finish_reason.as_deref()?;
    if !CLEAN_STOP_ALLOW.contains(&reason) {
        return None;
    }
    if choice.message.refusal.is_some() {
        return None;
    }
    Some(choice)
}

fn detect_structured_output(
    ctx: &DetectorContext,
    choice: &Choice,
) -> Option<CapabilityObservation> {
    if !ctx.strict_output_requested {
        return None;
    }

    match message_text(&choice.message).and_then(|text| parse_json(&text)) {
        Some(value) => {
            if required_keys_present(&value, &ctx.requested_schema_required_keys) {
                return Some(verified(STRUCTURED_OUTPUT, SCHEMA_PARSE));
            }
            Some(CapabilityObservation {
                capability_key: STRUCTURED_OUTPUT,
                evidence_class: SCHEMA_MISMATCH,
                direction: ObservationDirection::SuspectAbsence,
                tier: SignalTier::Inferred,
            })
        }
        // A strict-TOOL response carries its payload inside a tool_use
        // block's input rather than as message text. That shape is gated
        // here but NOT verified in v1 (shallow parse of a tool payload is
        // deferred), so it yields no observation. Prose or an unparseable
        // body with no tool_use block is a suspected absence.
        None if has_tool_use(&choice.message) => None,
        None => Some(suspect(STRUCTURED_OUTPUT, SCHEMA_MISMATCH)),
    }
}

fn detect_web_search(
    ctx: &DetectorContext,
    usage: Option<&Usage>,
    message: &Message,
) -> Option<CapabilityObservation> {
    if web_search_requests_positive(usage) || has_search_evidence_block(message) {
        return Some(verified(WEB_SEARCH, SEARCH_BLOCKS));
    }
    // tool_choice=auto with no evidence never produces an observation;
    // only a forced request does.
    if ctx.forced_web_search {
        return Some(suspect(WEB_SEARCH, SEARCH_ABSENT_FORCED));
    }
    None
}

fn detect_prompt_caching(usage: Option<&Usage>) -> Option<CapabilityObservation> {
    let usage = usage?;
    let created = usage.cache_creation_input_tokens.unwrap_or(0) > 0;
    let read = usage.cache_read_input_tokens.unwrap_or(0) > 0;
    let per_ttl = usage
        .cache_creation
        .as_ref()
        .is_some_and(cache_creation_populated);
    (created || read || per_ttl).then(|| verified(PROMPT_CACHING, CACHE_HIT))
}

fn detect_thinking(
    ctx: &DetectorContext,
    usage: Option<&Usage>,
    message: &Message,
) -> Option<CapabilityObservation> {
    if !ctx.reasoning_requested {
        return None;
    }
    // Thinking blocks are lifted to reasoning_details by the normalizer and
    // are never present as ContentPart, so this reads reasoning_details and
    // the usage counter -- not the content blocks.
    let details_present = !message.reasoning_details.is_empty();
    let tokens_positive = usage
        .and_then(|u| u.reasoning_tokens)
        .is_some_and(|n| n > 0);
    (details_present || tokens_positive).then(|| verified(THINKING, THINKING_BLOCKS))
}

/// Concatenated text of every text block in the assistant message, or
/// `None` when the message carries no text.
fn message_text(message: &Message) -> Option<String> {
    match &message.content {
        MessageContent::Text(text) => Some(text.clone()),
        MessageContent::Parts(parts) => {
            let mut buf = String::new();
            for part in parts {
                if let ContentPart::Known(KnownContentPart::Text { text, .. }) = part {
                    buf.push_str(text);
                }
            }
            (!buf.is_empty()).then_some(buf)
        }
        MessageContent::Null => None,
    }
}

/// Whether the message carries a tool-call request in either wire shape.
fn has_tool_use(message: &Message) -> bool {
    if message.tool_calls.as_ref().is_some_and(|c| !c.is_empty()) {
        return true;
    }
    matches!(&message.content, MessageContent::Parts(parts)
    if parts.iter().any(|p| matches!(
        p,
        ContentPart::Known(KnownContentPart::ToolUse { .. })
    )))
}

/// Parse the whole body as a single JSON value. Trailing prose after a
/// JSON value fails the parse, so a mixed body is not mistaken for
/// structured output.
fn parse_json(text: &str) -> Option<Value> {
    serde_json::from_str::<Value>(text).ok()
}

/// Whether every declared top-level required key is present. A non-object
/// body satisfies only an empty requirement (shallow shape check -- deep
/// conformance is a later probe's job).
fn required_keys_present(value: &Value, required: &[String]) -> bool {
    match value.as_object() {
        Some(object) => required.iter().all(|key| object.contains_key(key)),
        None => required.is_empty(),
    }
}

/// Whether the usage `server_tool_use` object reports a positive
/// web-search request count. Keys only on the search-specific counter --
/// an unrelated server tool's count must not credit web search.
fn web_search_requests_positive(usage: Option<&Usage>) -> bool {
    usage
        .and_then(|u| u.server_tool_use.as_ref())
        .and_then(Value::as_object)
        .is_some_and(|object| {
            WEB_SEARCH_REQUEST_KEYS.iter().any(|key| {
                object
                    .get(*key)
                    .and_then(Value::as_u64)
                    .is_some_and(|n| n > 0)
            })
        })
}

/// Whether the message carries a recognized web-search evidence block.
fn has_search_evidence_block(message: &Message) -> bool {
    let MessageContent::Parts(parts) = &message.content else {
        return false;
    };
    parts.iter().any(|part| {
        matches!(
            part,
            ContentPart::Other { type_tag, .. }
                if SEARCH_EVIDENCE_TYPE_TAGS.contains(&type_tag.as_str())
        )
    })
}

/// Whether a per-TTL cache-creation breakdown reports any tokens written.
fn cache_creation_populated(creation: &routectl_core::schema::CacheCreation) -> bool {
    creation.ephemeral_5m_input_tokens.unwrap_or(0) > 0
        || creation.ephemeral_1h_input_tokens.unwrap_or(0) > 0
}

/// A verified (`SelfIdentifying`) observation.
const fn verified(
    capability_key: &'static str,
    evidence_class: &'static str,
) -> CapabilityObservation {
    CapabilityObservation {
        capability_key,
        evidence_class,
        direction: ObservationDirection::Verified,
        tier: SignalTier::SelfIdentifying,
    }
}

/// A suspected-absence (`Inferred`) observation.
const fn suspect(
    capability_key: &'static str,
    evidence_class: &'static str,
) -> CapabilityObservation {
    CapabilityObservation {
        capability_key,
        evidence_class,
        direction: ObservationDirection::SuspectAbsence,
        tier: SignalTier::Inferred,
    }
}

#[cfg(test)]
#[path = "capability_detect_tests.rs"]
mod tests;
