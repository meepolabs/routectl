//! Canonical `messages[]` -> Responses `input[]` translation.
//!
//! The Responses API has no role-tagged message envelope: each
//! `input[]` item is a top-level tagged union (`message` / `reasoning`
//! / `function_call` / `function_call_output`). User and assistant
//! turns become `Message` items with `role: "user" | "assistant"` and
//! content blocks; assistant `thinking` parts become `Reasoning`
//! items (with the canonical signature lifted into
//! `encrypted_content`); tool messages become `FunctionCallOutput`
//! items. ToolUse content parts on an assistant turn become standalone
//! `FunctionCall` items emitted alongside the `Message` for that turn;
//! OpenAI-shape `Message.tool_calls` (populated by the OpenAI ingress
//! instead of ToolUse content parts) are re-emitted the same way so a
//! following `function_call_output` is never dangling.
//!
//! Content-part policy -- TWO classes, deliberately NOT unified:
//!
//! - MALFORMED: the caller asked to send bytes and named none (an
//!   `image_url` with no url or an empty one, an image source whose
//!   base64 `data` or `url` is empty, a file part carrying neither
//!   `file_data` nor `file_id`). There is no valid interpretation at any
//!   egress, so the request FAILS with a `normalize_request` error that
//!   names the offending field and content location. The error never
//!   echoes a caller-controlled type tag or any raw content value.
//! - UNREPRESENTABLE: the part is well-formed but this egress has no slot
//!   for it (a canonical part kind the Responses API does not model, a
//!   forward-compat part, an unknown image-source kind, a non-text part
//!   inside a tool result). The request is legitimate and serviceable, so
//!   the part DROPS with a WARN and the rest of the turn still ships;
//!   hard-failing here would turn working cross-dialect routes into 400s
//!   and make every new canonical part kind a breaking change at every
//!   egress that cannot emit it. The audience for that WARN is the
//!   operator, who can act on "this route cannot carry PDFs"; the caller
//!   is not told, because nothing was wrong with the request.
//!
//! The governing axis is malformed-vs-unrepresentable, NOT
//! recognized-vs-unrecognized: a recognized part with no carrier fails,
//! and an unrecognized but well-formed part drops.
//!
//! Reasoning replay: a reasoning item is emitted only when it carries a
//! non-empty `encrypted_content` this lane can validly replay. An item
//! with an empty signature has nothing to re-inject and its upstream id
//! is either a no-op or a hard "item not found" rejection, so no
//! translation path can produce one: every producer returns `Option` and
//! a final sweep before emission enforces the floor structurally.
//!
//! A `redacted_thinking` blob that crossed a dialect with no slot for a
//! reasoning artifact's id and scheme carries both in a self-describing
//! envelope, restored here so continuity survives the round trip. The
//! restored pair is a CLIENT-CONTROLLED hint and passes through the same
//! replay gate as a natively tagged artifact.
//!
//! Tool-call ids: every `call_id` this module emits -- on a
//! `function_call` and on the `function_call_output` answering it --
//! passes through `tool_id::sanitize_tool_id`. The Responses API
//! correlates an output to its call BY `call_id`, and sanitization is
//! pure, so applying it at all four emit sites is what keeps one logical
//! id landing on one wire id no matter which ingress shape it arrived in
//! (a `ToolUse` part or the OpenAI-shape `tool_calls` field for the call;
//! a `ToolResult` part on a user turn or a `Role::Tool` message for the
//! output). Sanitizing a subset silently drops the tool result: the
//! output names a call the request never contained, so the model answers
//! without it.
//!
//! OPEN: whether the Responses API constrains `call_id`'s charset at all
//! is UNVERIFIED -- the API reference is auth-walled and every `call_id`
//! in this repo's fixtures is synthetic and already wire-safe, so neither
//! source establishes a constraint. Sanitizing is correct either way,
//! because the two sides must MATCH regardless. If a live capture ever
//! shows `call_id` is permissive, passing ids through verbatim on this
//! lane becomes the more faithful option; that is a lane-awareness change
//! to the shared sanitizer, not a change here.

use serde_json::Value;

use routectl_core::{
    ContentPart, Error, KnownContentPart, Message, MessageContent, Replayability, Result, Role,
    is_replayable, is_responses_family, reasoning_envelope, sanitize_for_log, scheme_of,
};

use crate::bounded_diagnostics::BoundedLogSample;

use super::types::{
    FunctionCallOutputBody, FunctionCallOutputContentItem, ReasoningContentItem,
    ReasoningSummaryItem, ResponseInputItem, ResponsesContentItem,
};
use super::{AuthKind, lane_scheme};
use crate::translation_drop_metrics::record_translation_drop;
use routectl_core::{ReasoningDetail, ReasoningDetailKind};

/// Per-request tally of the deliberate content drops this egress makes
/// while building `input[]`.
///
/// Exactly one instance exists per request and it is flushed exactly once,
/// from [`build_input`], on both the Ok and the Err arm. That is what makes
/// the counters per-REQUEST rather than per-block: a turn carrying three
/// unrepresentable image sources is one drop event for the request, not
/// three.
///
/// Only the process-wide counters flush from here. Each arm keeps its own
/// WARN/DEBUG record at its own site, because those records carry
/// arm-specific structured fields (the offending source kind, the bounded
/// sample of foreign format tags) that a request-level aggregate has no
/// shape for.
#[derive(Default)]
struct ResponsesDropTally {
    image_source_kind: bool,
    reasoning_detail_kind: bool,
    reasoning_format_foreign: bool,
    reasoning_scheme_incompatible: bool,
}

impl ResponsesDropTally {
    const fn record_image_source_kind(&mut self) {
        self.image_source_kind = true;
    }

    const fn record_reasoning_detail_kind(&mut self) {
        self.reasoning_detail_kind = true;
    }

    const fn record_reasoning_format_foreign(&mut self) {
        self.reasoning_format_foreign = true;
    }

    const fn record_reasoning_scheme_incompatible(&mut self) {
        self.reasoning_scheme_incompatible = true;
    }

    /// Bump one process-wide counter per drop class this request hit at
    /// least once. The lane's request-volume denominator is NOT bumped
    /// here: `request::translate` owns the single `record_translation_lane_seen`
    /// site for this lane.
    ///
    /// The consequence, stated because it is easy to misread a drop count on
    /// this lane: in the TEST binary the numerator and denominator have
    /// different reachability. Roughly twenty in-crate test callers invoke
    /// `build_input` directly, so they move these drop counters without moving
    /// `lane_seen`. In production `translate` is the only caller, so the two
    /// agree there. A drop RATE computed inside the test binary is therefore
    /// not meaningful; a delta on a single class still is, which is what the
    /// pinning tests assert.
    fn flush(&self) {
        if self.image_source_kind {
            record_translation_drop("openai-responses", "image_source_kind_unrepresentable");
        }
        if self.reasoning_detail_kind {
            record_translation_drop("openai-responses", "reasoning_detail_kind_unsupported");
        }
        if self.reasoning_format_foreign {
            record_translation_drop("openai-responses", "reasoning_format_foreign");
        }
        if self.reasoning_scheme_incompatible {
            record_translation_drop("openai-responses", "reasoning_scheme_incompatible");
        }
    }
}

/// Walk the canonical `messages[]` and produce a flat `input[]` array
/// in Responses-shape. System messages are dropped here (lifted into
/// `instructions` by `system.rs`); each non-system turn may produce
/// 1+ input items (e.g. an assistant turn with both thinking and text
/// emits `Reasoning` + `Message`).
///
/// `auth_kind` names the lane this request is bound for: replay
/// portability is a property of the (artifact lane, target lane) pair,
/// so every reasoning artifact is gated against it before egress.
pub(super) fn build_input(
    id: &str,
    auth_kind: AuthKind,
    messages: &[Message],
) -> Result<Vec<ResponseInputItem>> {
    let mut tally = ResponsesDropTally::default();
    let out = build_input_tallied(id, auth_kind, messages, &mut tally);
    // Flushed before the `?`-free return so a request that FAILS after
    // dropping something still reports the drop it already made.
    tally.flush();
    out
}

fn build_input_tallied(
    id: &str,
    auth_kind: AuthKind,
    messages: &[Message],
    tally: &mut ResponsesDropTally,
) -> Result<Vec<ResponseInputItem>> {
    let mut out: Vec<ResponseInputItem> = Vec::with_capacity(messages.len());
    for msg in messages {
        match &msg.role {
            // A system turn's content is not lost here: `system.rs` reads
            // `req.system`, and BOTH ingresses that can reach this egress
            // hoist every `Role::System` message into that field before the
            // request leaves the ingress (`ingress::lift_system_messages`,
            // called unconditionally on the Responses and the OpenAI
            // ingress; the Anthropic ingress carries `system` as a
            // top-level field only). So no `Role::System` message survives
            // to reach this arm with content still on it, and the arm is a
            // structural filter rather than a drop.
            // TRANSLATION-DROP: structural -- system content is hoisted into `req.system` at ingress and emitted as `instructions`
            Role::System => {}
            Role::User => translate_user_message(id, msg, &mut out, tally)?,
            Role::Assistant => translate_assistant_message(id, auth_kind, msg, &mut out, tally)?,
            Role::Tool => translate_tool_message(id, msg, &mut out, tally)?,
            // This function serves callers whose ingress dialect is
            // openai_responses itself as well as callers translating in
            // from other dialects. Unlike Anthropic/Gemini/Converse, the
            // Responses wire role is a plain string with no closed
            // vocabulary, so the faithful move for both classes of caller
            // is to forward the original tag verbatim rather than
            // collapsing it onto "user" -- still logged once so a caller
            // translating in from elsewhere can see the choice made. This
            // is a forward-compat seed: not yet eligible for removal until
            // real unrecognized-role traffic is observed. Nothing is lost:
            // the tag rides to the wire verbatim and the turn's content
            // goes through the same tool-result lift and content build a
            // `Role::User` turn does.
            Role::Other(tag) => translate_other_message(id, tag, msg, &mut out, tally)?,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Per-role translation
// ---------------------------------------------------------------------------

fn translate_user_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
    tally: &mut ResponsesDropTally,
) -> Result<()> {
    // Lift any `tool_result` parts into FunctionCallOutput input items.
    // The Anthropic ingress emits tool outputs as
    // `ContentPart::ToolResult` on a user-role message (the Anthropic
    // wire shape); the Responses API needs them as separate
    // `function_call_output` input items keyed by call_id. Without
    // this lift, the upstream 400s with "No tool output found for
    // function call <id>".
    extract_tool_results(id, &msg.content, out)?;

    let content = build_user_content(id, &msg.content, tally)?;
    if content.is_empty() {
        tracing::debug!(
            provider = id,
            role = "user",
            "skipping empty user message after Responses translation"
        );
        return Ok(());
    }
    out.push(ResponseInputItem::Message {
        role: "user".into(),
        content,
    });
    Ok(())
}

/// An unrecognized-role turn: forwarded verbatim as its original wire
/// role string (see the `Role::Other` arm in `build_input`), reusing the
/// same tool-result lift and content build as `translate_user_message`
/// since Responses input items share one shape regardless of role tag.
fn translate_other_message(
    id: &str,
    tag: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
    tally: &mut ResponsesDropTally,
) -> Result<()> {
    tracing::debug!(
        provider = id,
        role = %sanitize_for_log(tag),
        "responses egress: unrecognized message role forwarded verbatim"
    );
    extract_tool_results(id, &msg.content, out)?;

    let content = build_user_content(id, &msg.content, tally)?;
    if content.is_empty() {
        tracing::debug!(
            provider = id,
            role = %sanitize_for_log(tag),
            "skipping empty message after Responses translation"
        );
        return Ok(());
    }
    out.push(ResponseInputItem::Message {
        role: tag.to_string(),
        content,
    });
    Ok(())
}

/// Walk a user-message content and emit one FunctionCallOutput input
/// item per `tool_result` part. The Anthropic Messages wire shape ships
/// tool outputs as user-turn content blocks; the Responses API wants
/// them as sibling input items, so we lift them out before
/// `build_user_content` walks the remaining parts.
///
/// An empty `tool_use_id` fails the request rather than dropping the
/// block. The Responses API correlates an output to its call BY
/// `call_id`: an empty id can never name the call it answers, so the
/// request would either be rejected upstream or answered without the
/// tool result. A loud local rejection beats a silently degraded answer,
/// and it is the same policy `translate_tool_message` applies to an
/// empty `tool_call_id` on the canonical `Role::Tool` shape -- one
/// defect, one policy, whichever ingress shape it arrived in.
fn extract_tool_results(
    id: &str,
    content: &MessageContent,
    out: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    let MessageContent::Parts(parts) = content else {
        return Ok(());
    };
    for p in parts {
        // Every non-ToolResult part is skipped by this pass ONLY: the
        // caller runs `build_user_content` over the same parts slice
        // immediately afterwards, which is where each of those parts is
        // translated or deliberately dropped with its own record. Nothing
        // is lost at this `continue`.
        // TRANSLATION-DROP: structural -- selects tool_result parts; the caller's content build walks the remaining parts
        let ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id,
            content,
            ..
        }) = p
        else {
            continue;
        };
        if tool_use_id.is_empty() {
            return Err(Error::normalize_request(
                id,
                "tool_result block has an empty tool_use_id (a ToolResult part \
                 requires a non-empty tool_use_id for the Responses API \
                 function_call_output item)",
            ));
        }
        let output = tool_result_to_output_body(id, content);
        out.push(ResponseInputItem::FunctionCallOutput {
            call_id: crate::tool_id::sanitize_tool_id(tool_use_id).into_owned(),
            output,
        });
    }
    Ok(())
}

/// Translate the Anthropic-shape `tool_result.content` value into a
/// FunctionCallOutputBody. Anthropic's content slot is permissive: a
/// flat string, an array of blocks, or any JSON value. Codex parity
/// prefers a flat string when possible.
fn tool_result_to_output_body(id: &str, content: &Value) -> FunctionCallOutputBody {
    if let Some(s) = content.as_str() {
        return FunctionCallOutputBody::Text(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        // Walk the array as if it were canonical parts. If every entry
        // is a `{type: "text", text: "..."}` block, collapse to a
        // flat string. Otherwise fall back to a JSON-encoded text body
        // so the upstream still sees the structured payload.
        let mut buf = String::new();
        let mut all_text = true;
        for v in arr {
            if let (Some("text"), Some(text)) = (
                v.get("type").and_then(Value::as_str),
                v.get("text").and_then(Value::as_str),
            ) {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            } else {
                all_text = false;
                break;
            }
        }
        if all_text {
            return FunctionCallOutputBody::Text(buf);
        }
    }
    // Anything else: serialize the value so the model gets the raw
    // structured output. Better than dropping the payload.
    let serialized = serde_json::to_string(content).unwrap_or_else(|e| {
        tracing::warn!(
            provider = id,
            error = %e,
            "tool_result content failed to serialize; emitting empty output"
        );
        String::new()
    });
    FunctionCallOutputBody::Text(serialized)
}

/// Assistant turn translation. Walks each part, splitting into a
/// reasoning-item stream + a message-content stream + a tool-call
/// stream, then emits items in the order [reasoning?, message?,
/// tool_calls...] so multi-turn replay preserves the original ordering
/// the model sees.
///
/// Reasoning replay (critical correctness path): the Responses-side
/// canonical channel for prior-turn reasoning is
/// `msg.reasoning_details` (the response translator stamps every
/// reasoning block there with the producing lane's format tag and
/// preserves the upstream `encrypted_content` signature). Routing
/// reasoning solely through content-parts would lose the signature
/// because `ContentPart::Thinking` has no slot for the JWT payload.
///
/// Mutual-exclusion rule: `reasoning_details` (the response-side
/// channel) and `ContentPart::Thinking` (the request-side channel)
/// MUST NOT both populate on the same assistant turn. When they do,
/// prefer `reasoning_details` and skip Thinking parts to avoid
/// duplicate Reasoning items on the wire. Log at debug so the
/// duplicate is visible during triage.
fn translate_assistant_message(
    id: &str,
    auth_kind: AuthKind,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
    tally: &mut ResponsesDropTally,
) -> Result<()> {
    let mut reasoning_items: Vec<ResponseInputItem> = Vec::new();
    let mut message_content: Vec<ResponsesContentItem> = Vec::new();
    let mut tool_calls: Vec<ResponseInputItem> = Vec::new();

    // First, lift reasoning_details into Reasoning input items.
    // Only entries tagged with a Responses-family format participate;
    // other formats (e.g. Anthropic) ride a different replay shape that
    // the canonical hub doesn't translate here.
    lift_reasoning_details(
        &msg.reasoning_details,
        auth_kind,
        &mut reasoning_items,
        tally,
    );
    let suppress_thinking_parts = !reasoning_items.is_empty();

    let mut content_has_tool_use = false;
    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => {
            message_content.push(ResponsesContentItem::OutputText { text: t.clone() });
        }
        // An empty-string or Null assistant content carries no text to
        // emit, and a Responses `message` item with an empty `content`
        // array is what the arm below would produce -- which the caller
        // already declines to push. A reasoning-only or tool-call-only
        // assistant turn is legitimate and its other surfaces still ship.
        // TRANSLATION-DROP: structural -- an empty or null assistant content has no text to carry
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            content_has_tool_use = parts
                .iter()
                .any(|p| matches!(p, ContentPart::Known(KnownContentPart::ToolUse { .. })));
            for p in parts {
                walk_assistant_part(
                    id,
                    auth_kind,
                    p,
                    suppress_thinking_parts,
                    &mut reasoning_items,
                    &mut message_content,
                    &mut tool_calls,
                )?;
            }
        }
    }

    // Re-emit OpenAI-shape `Message.tool_calls` as `function_call` input
    // items. The OpenAI ingress populates `tool_calls` rather than
    // emitting `KnownContentPart::ToolUse` content parts; without this a
    // turn whose calls live only on `tool_calls` produces no
    // `function_call`, and the following `function_call_output` is
    // dangling ("No tool output found for function call <id>"). The guard
    // skips re-emission when content already carried ToolUse parts (the
    // walk above already pushed those) so the call isn't doubled. The
    // Responses wire wants `arguments` as a JSON STRING, so the parsed
    // value is re-serialized -- consistent with the ToolUse-part path,
    // which also `serde_json::to_string`s its input.
    append_function_calls_from_tool_calls(id, msg, content_has_tool_use, &mut tool_calls)?;

    retain_replayable_reasoning(&mut reasoning_items);
    out.extend(reasoning_items);
    if !message_content.is_empty() {
        out.push(ResponseInputItem::Message {
            role: "assistant".into(),
            content: message_content,
        });
    }
    out.extend(tool_calls);
    Ok(())
}

/// Final structural gate on the reasoning items an assistant turn is
/// about to emit: a Reasoning item whose `encrypted_content` is empty
/// carries nothing replayable, and re-injecting it by its upstream id is
/// either a no-op or a hard "item not found" rejection.
///
/// Every producer already declines to build such an item. This sweep is
/// deliberately redundant: it makes the floor a property of the emission
/// point rather than of each producer, so a future path cannot reintroduce
/// the hole by forgetting the check at its own site.
pub(super) fn retain_replayable_reasoning(items: &mut Vec<ResponseInputItem>) {
    items.retain(|item| {
        !matches!(
            item,
            ResponseInputItem::Reasoning {
                encrypted_content,
                ..
            } if encrypted_content.is_empty()
        )
    });
}

/// Re-emit OpenAI-shape `Message.tool_calls` as Responses `function_call`
/// input items. See `translate_assistant_message` for the orphaned-output
/// failure this prevents. No-op when `tool_calls` is empty or when the
/// content already carried `ToolUse` parts (avoids double-emission).
fn append_function_calls_from_tool_calls(
    id: &str,
    msg: &Message,
    content_has_tool_use: bool,
    tool_calls: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    let Some(raw_calls) = msg.tool_calls.as_ref().filter(|tc| !tc.is_empty()) else {
        return Ok(());
    };
    if content_has_tool_use {
        return Ok(());
    }
    for call in crate::tool_calls::normalize_tool_calls(id, raw_calls) {
        tool_calls.push(ResponseInputItem::FunctionCall {
            call_id: call.id,
            name: call.name,
            arguments: serde_json::to_string(&call.arguments)
                .map_err(|e| Error::normalize_request(id, e.to_string()))?,
        });
    }
    Ok(())
}

/// Walk `reasoning_details` and emit one Reasoning input item per
/// distinct upstream item id (or one fall-through item when no id is
/// preserved). Multiple details sharing the same `id` collapse to a
/// single Reasoning item carrying the union of summary, content, and
/// encrypted_content surfaces.
///
/// Two independent gates run per detail:
///
/// - RECOGNITION: only Responses-family tags participate. Anything else
///   (e.g. Anthropic) comes from a different upstream and replaying it
///   here would corrupt the wire. The check goes through
///   `is_responses_family` rather than an equality test against one tag,
///   so a newly minted lane tag is recognized instead of silently
///   vanishing.
/// - REPLAY: proven-incompatible (artifact scheme, lane scheme) pairs are
///   stripped -- the upstream validator rejects them outright. Pairs that
///   are not established either way are carried OPTIMISTICALLY: carrying
///   once is how an unproven pair gets settled, and a rejection is the
///   router's to learn from. This layer never retries and never learns.
fn lift_reasoning_details(
    details: &[ReasoningDetail],
    auth_kind: AuthKind,
    out: &mut Vec<ResponseInputItem>,
    tally: &mut ResponsesDropTally,
) {
    if details.is_empty() {
        return;
    }
    // Bucket by id (None-id details ride a single unnamed bucket).
    // Preserve arrival order via the `order` vector so output is
    // deterministic.
    let mut order: Vec<Option<String>> = Vec::new();
    let mut groups: std::collections::HashMap<Option<String>, ReasoningGroup> =
        std::collections::HashMap::new();
    let mut skipped_count: u32 = 0;
    // Sanitized before the distinctness test so each entry is
    // length-bounded at collection time and tags differing only in
    // control characters share one slot.
    let mut skipped_formats: BoundedLogSample<String> = BoundedLogSample::new();
    let mut stripped_count: u32 = 0;
    let mut unsupported_kind_count: u32 = 0;
    // The `Other` discriminator is CLIENT-CONTROLLED, so it is sanitized
    // before the distinctness test -- same collection-time bounding the
    // format sample uses, for the same log-injection reason.
    let mut unsupported_kinds: BoundedLogSample<String> = BoundedLogSample::new();
    let lane = lane_scheme(auth_kind);

    for d in details {
        let format = d.format.as_deref();
        // A foreign-tagged detail was minted by an upstream in a different
        // dialect; its payload is not a token this egress's `reasoning`
        // item can carry, and forwarding it corrupts the upstream's replay
        // gate. Cross-dialect only: the Responses ingress stamps
        // `openai-responses-v1` on every detail it produces, which is a
        // Responses-family tag, so a same-dialect request never reaches
        // this gate. Seed per foundations sec 14, deletion-blocked pending
        // per-lane wire evidence.
        // TRANSLATION-DROP: lane=openai-responses class=reasoning_format_foreign test=foreign_format_detail_drops_from_the_wire_and_counts
        if !is_responses_family(format) {
            skipped_count += 1;
            skipped_formats.push_distinct(sanitize_for_log(format.unwrap_or("<none>")));
            tally.record_reasoning_format_foreign();
            continue;
        }
        // A Responses-family artifact whose replay scheme the TARGET lane
        // is proven to reject: the upstream validator refuses it outright,
        // so shipping it turns a serviceable request into a 400. Only a
        // PROVEN-incompatible pair is stripped; an unestablished pair is
        // carried optimistically by the same gate. Same-dialect reachable
        // (a Responses client's own artifact steered at a different
        // Responses lane), but nothing is lost that the far side would
        // have accepted -- the far side is the one rejecting it. Seed per
        // foundations sec 14, deletion-blocked pending per-lane evidence.
        // TRANSLATION-DROP: lane=openai-responses class=reasoning_scheme_incompatible test=scheme_incompatible_detail_drops_from_the_wire_and_counts
        if is_replayable(scheme_of(format), lane) == Replayability::Strip {
            stripped_count += 1;
            tally.record_reasoning_scheme_incompatible();
            continue;
        }
        let key = d.id.clone();
        if !groups.contains_key(&key) {
            order.push(key.clone());
            groups.insert(key.clone(), ReasoningGroup::default());
        }
        let group = groups.get_mut(&key).expect("inserted above");
        match &d.kind {
            ReasoningDetailKind::Summary => {
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    group.summary.push(ReasoningSummaryItem::SummaryText {
                        text: text.to_string(),
                    });
                }
            }
            ReasoningDetailKind::Text => {
                if let Some(text) = d.payload.get("text").and_then(|v| v.as_str()) {
                    group.content.push(ReasoningContentItem::ReasoningText {
                        text: text.to_string(),
                    });
                }
            }
            ReasoningDetailKind::Encrypted => {
                if let Some(sig) = d.payload.get("encrypted_content").and_then(|v| v.as_str()) {
                    if group.encrypted_content.is_none() {
                        group.encrypted_content = Some(sig.to_string());
                    } else {
                        // Multiple Encrypted details on the same id:
                        // surface as an inner reasoning_encrypted
                        // content block so no signature is lost.
                        group
                            .content
                            .push(ReasoningContentItem::ReasoningEncrypted {
                                encrypted_content: sig.to_string(),
                            });
                    }
                }
            }
            // Unrecognized kind: this egress's `reasoning` item has no
            // slot for an arbitrary shape (summary/content/
            // encrypted_content are the complete set), so it contributes
            // nothing to the group -- the same forward-shaped no-op every
            // arm above already applies when its own expected payload
            // field is absent.
            //
            // Cross-dialect only. The Responses ingress is the sole
            // same-dialect producer of `reasoning_details`, and both of its
            // constructing helpers emit only Summary / Encrypted / Text --
            // an unknown inner reasoning-content type dies at the inner
            // helper's own catch-all before a detail is ever built. An
            // `Other` kind therefore arises only from `Deserialize` mapping
            // an unrecognized discriminator on the canonical schema, which
            // a Chat Completions client reaches by deserializing
            // `ChatRequest` wholesale. Seed per foundations sec 14,
            // deletion-blocked pending per-lane wire evidence.
            // TRANSLATION-DROP: lane=openai-responses class=reasoning_detail_kind_unsupported test=unrecognized_detail_kind_drops_from_the_wire_and_counts
            ReasoningDetailKind::Other(tag) => {
                unsupported_kind_count += 1;
                unsupported_kinds.push_distinct(sanitize_for_log(tag));
                tally.record_reasoning_detail_kind();
            }
        }
    }

    let mut empty_encrypted_count: u32 = 0;
    for key in order {
        let group = groups.remove(&key).expect("recorded in order");
        let encrypted_content = group.encrypted_content.unwrap_or_default();
        // A reasoning item with empty encrypted_content cannot be
        // validly replayed: re-injecting it by its upstream id is a
        // no-op (chatgpt-oauth) or a hard 404 "Item not found"
        // (api.openai.com). Skip it rather than ship a dangling id.
        // The upstream item id is a reasoning-replay artifact and must
        // never reach a log line at any level -- count the skips and
        // emit a bounded aggregate instead of the id itself.
        //
        // OPEN, and why this is a fidelity risk rather than an accepted
        // drop: the skip discards the group's `summary` and `content`
        // surfaces along with the unreplayable id, and it is SAME-DIALECT
        // REACHABLE. The Responses ingress attaches an `Encrypted` detail
        // only when the inbound item carried a non-empty
        // `encrypted_content`, so a Responses client echoing back a
        // summary-only reasoning item loses that summary here. Whether an
        // id-less summary-only `reasoning` item is accepted by either lane
        // -- which would make forwarding the summary the faithful move --
        // is UNVERIFIED against a live upstream. Until it is, this arm is
        // not documented as an acceptable translation drop.
        // TRANSLATION-DROP: fidelity-risk -- same-dialect reachable: a summary-only reasoning item loses its summary along with the unreplayable id
        if encrypted_content.is_empty() {
            empty_encrypted_count += 1;
            continue;
        }
        out.push(ResponseInputItem::Reasoning {
            id: key,
            summary: group.summary,
            content: group.content,
            encrypted_content,
        });
    }
    if empty_encrypted_count > 0 {
        tracing::debug!(
            skipped_empty_encrypted = empty_encrypted_count,
            "openai-responses: skipped reasoning replay item(s) with empty encrypted_content"
        );
    }

    if skipped_count > 0 {
        tracing::debug!(
            skipped = skipped_count,
            formats = ?skipped_formats.items(),
            formats_truncated = skipped_formats.truncated(),
            "openai-responses: skipped reasoning_details entries with a non-Responses-family format"
        );
    }
    if stripped_count > 0 {
        tracing::debug!(
            stripped = stripped_count,
            lane = ?lane,
            "openai-responses: stripped reasoning_details entries whose replay scheme the target lane rejects"
        );
    }
    if unsupported_kind_count > 0 {
        tracing::debug!(
            dropped = unsupported_kind_count,
            kinds = ?unsupported_kinds.items(),
            kinds_truncated = unsupported_kinds.truncated(),
            "openai-responses: dropped reasoning_details entries whose kind has no Responses reasoning-item slot"
        );
    }
}

#[derive(Default)]
struct ReasoningGroup {
    summary: Vec<ReasoningSummaryItem>,
    content: Vec<ReasoningContentItem>,
    encrypted_content: Option<String>,
}

/// Translate a canonical `Role::Tool` message into a
/// `function_call_output` input item.
///
/// A missing or empty `tool_call_id` fails the request. The Responses API
/// correlates an output to its call BY `call_id`, so an empty id can
/// never name the call it answers and the model would answer without the
/// tool result. `extract_tool_results` applies the same policy to an
/// empty `tool_use_id` on the Anthropic-shape lane: one defect, one
/// policy, so the outcome does not depend on the ingress shape.
fn translate_tool_message(
    id: &str,
    msg: &Message,
    out: &mut Vec<ResponseInputItem>,
    tally: &mut ResponsesDropTally,
) -> Result<()> {
    let Some(raw_call_id) = msg.tool_call_id.as_deref().filter(|s| !s.is_empty()) else {
        return Err(Error::normalize_request(
            id,
            "tool message has no usable tool_call_id: the field is absent or empty \
             (Role::Tool requires a non-empty tool_call_id for the Responses API \
             function_call_output item)",
        ));
    };
    let call_id = crate::tool_id::sanitize_tool_id(raw_call_id).into_owned();
    let output = match &msg.content {
        MessageContent::Text(t) => FunctionCallOutputBody::Text(t.clone()),
        MessageContent::Null => FunctionCallOutputBody::Text(String::new()),
        MessageContent::Parts(parts) => build_tool_output_body(id, parts, tally)?,
    };
    out.push(ResponseInputItem::FunctionCallOutput { call_id, output });
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-part translation
// ---------------------------------------------------------------------------

fn build_user_content(
    id: &str,
    content: &MessageContent,
    tally: &mut ResponsesDropTally,
) -> Result<Vec<ResponsesContentItem>> {
    match content {
        MessageContent::Text(t) if t.is_empty() => Ok(Vec::new()),
        MessageContent::Text(t) => Ok(vec![ResponsesContentItem::InputText { text: t.clone() }]),
        MessageContent::Null => Ok(Vec::new()),
        MessageContent::Parts(parts) => {
            let mut out: Vec<ResponsesContentItem> = Vec::with_capacity(parts.len());
            for p in parts {
                match p {
                    ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                        if !text.is_empty() {
                            out.push(ResponsesContentItem::InputText { text: text.clone() });
                        }
                    }
                    ContentPart::Known(KnownContentPart::Image { source, .. }) => {
                        if let Some(item) = translate_image_source(id, source, tally)? {
                            out.push(item);
                        }
                    }
                    ContentPart::Known(KnownContentPart::ImageUrl { image_url, .. }) => {
                        // OpenAI-shape image_url block: extract the url
                        // field and emit an InputImage. detail, if present
                        // on the nested object, is forwarded. A url that is
                        // absent OR present-but-empty is malformed: an
                        // empty `image_url` names no bytes, so it fails
                        // rather than shipping upstream.
                        let Some(url) = image_url
                            .get("url")
                            .and_then(|u| u.as_str())
                            .filter(|s| !s.is_empty())
                        else {
                            return Err(Error::normalize_request(
                                id,
                                "image_url content part on a user message has no usable url: \
                                 image_url.url is absent or empty",
                            ));
                        };
                        let detail = image_url
                            .get("detail")
                            .and_then(|d| d.as_str())
                            .map(str::to_string);
                        out.push(ResponsesContentItem::InputImage {
                            image_url: url.to_string(),
                            detail,
                        });
                    }
                    ContentPart::Known(KnownContentPart::ToolResult { .. }) => {
                        // Lifted to FunctionCallOutput in
                        // `extract_tool_results`; skip silently here.
                    }
                    ContentPart::Known(KnownContentPart::File { file, .. }) => {
                        out.push(translate_file_part(id, file)?);
                    }
                    ContentPart::Known(other) => {
                        tracing::warn!(
                            provider = id,
                            part_type = other.type_tag(),
                            role = "user",
                            "dropping unsupported user content part on Responses egress"
                        );
                    }
                    ContentPart::Other { type_tag, .. } => {
                        tracing::warn!(
                            provider = id,
                            part_type = %sanitize_for_log(type_tag),
                            role = "user",
                            "dropping forward-compat user content part on Responses egress"
                        );
                    }
                }
            }
            Ok(out)
        }
    }
}

/// Per-part walker for an assistant turn. Routes each part into the
/// appropriate output bucket: `Thinking` -> reasoning items;
/// `RedactedThinking` -> a restored reasoning item when the blob is a
/// self-describing envelope, otherwise nothing (an opaque foreign blob is
/// not a valid token for the `encrypted_content` slot); `Text` -> message
/// content (output_text); `ToolUse` -> a separate `FunctionCall` input
/// item. Everything else drops with a WARN.
///
/// `suppress_thinking_parts` is true when `reasoning_details` already
/// produced Reasoning items: in that case Thinking + RedactedThinking
/// content parts are skipped (with a debug log) to avoid duplicate
/// Reasoning items on the wire. The canonical schema invariant is
/// that the two surfaces are mutually exclusive; we prefer the
/// response-side `reasoning_details` because it carries the JWT
/// signature in a slot that ContentPart::Thinking lacks.
fn walk_assistant_part(
    id: &str,
    auth_kind: AuthKind,
    p: &ContentPart,
    suppress_thinking_parts: bool,
    reasoning: &mut Vec<ResponseInputItem>,
    message_content: &mut Vec<ResponsesContentItem>,
    tool_calls: &mut Vec<ResponseInputItem>,
) -> Result<()> {
    match p {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => {
            if !text.is_empty() {
                message_content.push(ResponsesContentItem::OutputText { text: text.clone() });
            }
        }
        ContentPart::Known(KnownContentPart::Thinking {
            thinking,
            signature,
        }) => {
            if suppress_thinking_parts {
                tracing::debug!(
                    provider = id,
                    role = "assistant",
                    "skipping Thinking content-part because reasoning_details already emitted Reasoning items"
                );
            } else {
                reasoning.extend(translate_thinking_part(
                    thinking,
                    signature.as_deref(),
                    None,
                    auth_kind,
                ));
            }
        }
        ContentPart::Known(KnownContentPart::RedactedThinking { data }) => {
            if suppress_thinking_parts {
                tracing::debug!(
                    provider = id,
                    role = "assistant",
                    "skipping RedactedThinking content-part because reasoning_details already emitted Reasoning items"
                );
            } else {
                // A RedactedThinking content-part carries an opaque blob
                // with no format tag of its own. When it is a
                // Responses-family artifact that crossed a dialect with no
                // slot for its id and scheme, both ride inside a
                // self-describing envelope and are restored here --
                // without which the artifact is unreplayable on the lane
                // that issued it and reasoning continuity is lost.
                //
                // The restored (scheme, id) is CLIENT-CONTROLLED and is a
                // HINT ONLY. It is fed into exactly the same replay gate a
                // natively tagged detail passes through, so a claim can
                // never be what admits a blob to a lane. A non-envelope
                // blob -- an Anthropic-native redacted_thinking above all
                // -- restores nothing and is not forwarded: it is not a
                // valid token for this slot.
                reasoning.extend(decode_redacted_thinking(data, auth_kind));
            }
        }
        ContentPart::Known(KnownContentPart::ToolUse {
            id: tu_id,
            name,
            input,
            ..
        }) => {
            tool_calls.push(ResponseInputItem::FunctionCall {
                call_id: crate::tool_id::sanitize_tool_id(tu_id).into_owned(),
                name: name.clone(),
                arguments: serde_json::to_string(input)
                    .map_err(|e| Error::normalize_request(id, e.to_string()))?,
            });
        }
        ContentPart::Known(other) => {
            tracing::warn!(
                provider = id,
                part_type = other.type_tag(),
                role = "assistant",
                "dropping unsupported assistant content part on Responses egress"
            );
        }
        ContentPart::Other { type_tag, .. } => {
            tracing::warn!(
                provider = id,
                part_type = %sanitize_for_log(type_tag),
                role = "assistant",
                "dropping forward-compat assistant content part on Responses egress"
            );
        }
    }
    Ok(())
}

/// The signature to forward as `encrypted_content`, or `None` when there
/// is nothing this lane can validly replay.
///
/// A signature survives only when its source `format` is a
/// Responses-family tag whose replay scheme the target lane does not
/// reject. Otherwise it is not a token this lane's validator will accept,
/// and forwarding it corrupts the replay gate on the upstream server.
///
/// An unproven (artifact, lane) pair is carried optimistically: this layer
/// neither retries nor learns, and carrying once is what lets the pair be
/// settled from a real upstream verdict.
///
/// An absent or empty signature is `None` for the same reason a rejected
/// one is: an item whose `encrypted_content` is empty has no replayable
/// content, and re-injecting it by its upstream id is either a no-op or a
/// hard "item not found" rejection.
fn replayable_signature(
    signature: Option<&str>,
    format: Option<&str>,
    auth_kind: AuthKind,
) -> Option<String> {
    let replayable = is_responses_family(format)
        && is_replayable(scheme_of(format), lane_scheme(auth_kind)) != Replayability::Strip;
    if !replayable {
        return None;
    }
    signature.filter(|s| !s.is_empty()).map(str::to_string)
}

/// Restore a reasoning artifact from a `redacted_thinking` blob that
/// crossed a dialect with no slot for its id and scheme.
///
/// Returns `None` for every blob that is not a well-formed envelope --
/// a dialect-native redacted blob, a truncated or malformed envelope, an
/// unknown envelope version -- which leaves the caller holding an opaque
/// foreign blob and emitting no item, exactly as it would without this
/// decode.
///
/// SECURITY: the envelope is CLIENT-CONTROLLED. Anyone can mint one
/// claiming any scheme and any id, so the unwrapped pair is a HINT and
/// never an authorization. The claimed scheme is fed into the same replay
/// gate a natively tagged detail passes through, which means a claim of a
/// scheme the target lane rejects strips the artifact regardless of what
/// was claimed. The gate decides; the claim never does.
///
/// The blob rides through byte-identical to the bytes the provider
/// originally issued, so prompt-cache affinity upstream is untouched.
fn decode_redacted_thinking(data: &str, auth_kind: AuthKind) -> Option<ResponseInputItem> {
    let (claimed_scheme, claimed_id, blob) = reasoning_envelope::unwrap(data)?;
    let encrypted_content = replayable_signature(Some(blob), Some(claimed_scheme), auth_kind)?;
    Some(ResponseInputItem::Reasoning {
        id: claimed_id.map(str::to_string),
        summary: Vec::new(),
        content: Vec::new(),
        encrypted_content,
    })
}

/// Translate a canonical Thinking block to a Responses-shape `Reasoning`
/// input item, or `None` when the block has no signature this lane can
/// replay (see [`replayable_signature`]).
///
/// Returning `Option` rather than an item with empty `encrypted_content`
/// is what keeps the empty-item floor structural: the walk path and the
/// `reasoning_details` lift path cannot drift apart on whether an
/// unreplayable item is emitted, because no producer can build one.
pub(super) fn translate_thinking_part(
    thinking: &str,
    signature: Option<&str>,
    format: Option<&str>,
    auth_kind: AuthKind,
) -> Option<ResponseInputItem> {
    let encrypted_content = replayable_signature(signature, format, auth_kind)?;
    let summary = if thinking.is_empty() {
        Vec::new()
    } else {
        vec![ReasoningSummaryItem::SummaryText {
            text: thinking.to_string(),
        }]
    };
    Some(ResponseInputItem::Reasoning {
        id: None,
        summary,
        content: Vec::new(),
        encrypted_content,
    })
}

/// Translate an OpenAI-shape `File` part's nested `file` object into a
/// `ResponsesContentItem::InputFile`. The nested object carries either
/// `file_data` (a `data:<mime>;base64,<...>` URI for an inline upload)
/// or `file_id` (a reference to a previously-uploaded file), plus an
/// optional `filename`. A part carrying neither is malformed -- it names
/// no bytes the upstream can act on -- so it fails the request.
fn translate_file_part(id: &str, file: &serde_json::Value) -> Result<ResponsesContentItem> {
    let file_data = file
        .get("file_data")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let file_id = file
        .get("file_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let filename = file
        .get("filename")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if file_data.is_none() && file_id.is_none() {
        return Err(Error::normalize_request(
            id,
            "file content part on a user message has no usable carrier: \
             file.file_data and file.file_id are both absent or empty",
        ));
    }

    Ok(ResponsesContentItem::InputFile {
        file_data,
        file_id,
        filename,
    })
}

/// Translate a canonical `Image` source block to a
/// `ResponsesContentItem::InputImage`.
///
/// An empty base64 `data` or an empty `url` is malformed and fails the
/// request. An unrecognized source shape is a forward-compat extension
/// this egress cannot represent, so it yields `Ok(None)` and a WARN.
///
/// The unrecognized-kind drop is CROSS-DIALECT ONLY. The Responses ingress
/// produces no `KnownContentPart::Image` at all -- an `input_image` block
/// becomes the OpenAI-shape `ImageUrl` carrier, which takes the sibling arm
/// in `build_user_content` and never reaches here. An `Image` part with a
/// `source.type` outside `base64` / `url` therefore arrives only from an
/// Anthropic-shape or forward-compat client. Seed per foundations sec 14,
/// deletion-blocked pending per-lane wire evidence.
fn translate_image_source(
    id: &str,
    source: &serde_json::Value,
    tally: &mut ResponsesDropTally,
) -> Result<Option<ResponsesContentItem>> {
    let kind = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                return Err(Error::normalize_request(
                    id,
                    "image content part on a user message has empty base64 payload: \
                     source.data is absent or empty",
                ));
            }
            Ok(Some(ResponsesContentItem::InputImage {
                image_url: format!("data:{media_type};base64,{data}"),
                detail: None,
            }))
        }
        "url" => {
            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if url.is_empty() {
                return Err(Error::normalize_request(
                    id,
                    "image content part on a user message has no usable url: \
                     source.url is absent or empty",
                ));
            }
            Ok(Some(ResponsesContentItem::InputImage {
                image_url: url.to_string(),
                detail: None,
            }))
        }
        // TRANSLATION-DROP: lane=openai-responses class=image_source_kind_unrepresentable test=unknown_user_image_source_kind_drops_from_the_wire_and_counts
        other => {
            tracing::warn!(
                provider = id,
                source_kind = other,
                role = "user",
                "dropping image part with unknown source kind on Responses egress"
            );
            tally.record_image_source_kind();
            Ok(None)
        }
    }
}

/// Build a `FunctionCallOutputBody` from a parts slice. When all parts
/// are plain text the result collapses to a flat string (codex parity,
/// most-common path). When any part is non-text (e.g. an image returned
/// by a visual tool) the result is an Items array. Parts this egress
/// cannot represent are WARN-dropped and the remaining known parts are
/// still forwarded; a malformed image part fails the request.
fn build_tool_output_body(
    id: &str,
    parts: &[ContentPart],
    tally: &mut ResponsesDropTally,
) -> Result<FunctionCallOutputBody> {
    let has_non_text = parts.iter().any(|p| {
        matches!(
            p,
            ContentPart::Known(KnownContentPart::Image { .. } | KnownContentPart::ImageUrl { .. })
        )
    });

    if !has_non_text {
        // Fast path: all text. Concatenate.
        let mut buf = String::new();
        for p in parts {
            if let ContentPart::Known(KnownContentPart::Text { text, .. }) = p {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(text);
            } else {
                tracing::warn!(
                    provider = id,
                    part_type = p.type_tag(),
                    role = "tool",
                    "dropping unsupported tool result part on Responses egress"
                );
            }
        }
        return Ok(FunctionCallOutputBody::Text(buf));
    }

    // Mixed path: build typed items array.
    let mut items: Vec<FunctionCallOutputContentItem> = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            ContentPart::Known(KnownContentPart::Text { text, .. }) => {
                items.push(FunctionCallOutputContentItem::InputText { text: text.clone() });
            }
            ContentPart::Known(KnownContentPart::Image { source, .. }) => {
                if let Some(item) = translate_tool_image_source(id, source, tally)? {
                    items.push(item);
                }
            }
            ContentPart::Known(KnownContentPart::ImageUrl { image_url, .. }) => {
                // A url that is absent OR present-but-empty names no
                // bytes; failing keeps an empty `image_url` off the wire.
                let Some(url) = image_url
                    .get("url")
                    .and_then(|u| u.as_str())
                    .filter(|s| !s.is_empty())
                else {
                    return Err(Error::normalize_request(
                        id,
                        "image_url content part in a tool result has no usable url: \
                         image_url.url is absent or empty",
                    ));
                };
                let detail = image_url
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .map(str::to_string);
                items.push(FunctionCallOutputContentItem::InputImage {
                    image_url: url.to_string(),
                    detail,
                });
            }
            other => {
                tracing::warn!(
                    provider = id,
                    part_type = other.type_tag(),
                    role = "tool",
                    "dropping unsupported tool result part on Responses egress"
                );
            }
        }
    }
    Ok(FunctionCallOutputBody::Items(items))
}

/// Translate an Anthropic-shape image source inside a tool result to a
/// `FunctionCallOutputContentItem::InputImage`.
///
/// An empty base64 `data` or an empty `url` is malformed and fails the
/// request; an unrecognized source kind yields `Ok(None)` and a WARN.
///
/// Same reachability as [`translate_image_source`]: cross-dialect only.
/// The Responses ingress models a tool output's content blocks through the
/// same `parse_content_block` walk, which mints the OpenAI-shape `ImageUrl`
/// carrier for `input_image` and never a `KnownContentPart::Image`. Seed per
/// foundations sec 14, deletion-blocked pending per-lane wire evidence.
fn translate_tool_image_source(
    id: &str,
    source: &serde_json::Value,
    tally: &mut ResponsesDropTally,
) -> Result<Option<FunctionCallOutputContentItem>> {
    let kind = source.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "base64" => {
            let media_type = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("application/octet-stream");
            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if data.is_empty() {
                return Err(Error::normalize_request(
                    id,
                    "image content part in a tool result has empty base64 payload: \
                     source.data is absent or empty",
                ));
            }
            Ok(Some(FunctionCallOutputContentItem::InputImage {
                image_url: format!("data:{media_type};base64,{data}"),
                detail: None,
            }))
        }
        "url" => {
            let url = source.get("url").and_then(|u| u.as_str()).unwrap_or("");
            if url.is_empty() {
                return Err(Error::normalize_request(
                    id,
                    "image content part in a tool result has no usable url: \
                     source.url is absent or empty",
                ));
            }
            Ok(Some(FunctionCallOutputContentItem::InputImage {
                image_url: url.to_string(),
                detail: None,
            }))
        }
        // TRANSLATION-DROP: lane=openai-responses class=image_source_kind_unrepresentable test=unknown_tool_result_image_source_kind_drops_from_the_wire_and_counts
        other => {
            tracing::warn!(
                provider = id,
                source_kind = other,
                role = "tool",
                "dropping image part with unknown source kind in tool result on Responses egress"
            );
            tally.record_image_source_kind();
            Ok(None)
        }
    }
}

#[cfg(test)]
mod messages_tests {
    use serde_json::json;

    use routectl_core::{
        BEDROCK_MANTLE, CODEX_OAUTH, OPENAI_APIKEY, OPENAI_RESPONSES_V1, ReasoningDetail,
        ReasoningDetailKind,
    };

    use super::super::types::ResponseInputItem;
    use super::super::{AuthKind, OPENAI_RESPONSES_FORMAT};
    use super::{ResponsesDropTally, lift_reasoning_details, translate_thinking_part};
    use crate::bounded_diagnostics::MAX_LOGGED_DIAGNOSTIC_ITEMS;

    /// Drive the lift with a throwaway tally that is never flushed, so a
    /// test exercising the lift in isolation asserts its OUTPUT and its LOG
    /// records without touching the process-global drop registry -- which is
    /// what keeps these tests free of a serial guard. The counter-delta
    /// assertions live in the drop-policy fragment, which drives the lift
    /// through `build_input` (the site that does flush) instead.
    fn lift(details: &[ReasoningDetail], lane: AuthKind, out: &mut Vec<ResponseInputItem>) {
        let mut tally = ResponsesDropTally::default();
        lift_reasoning_details(details, lane, out, &mut tally);
    }

    fn make_detail(
        format: Option<&str>,
        kind: ReasoningDetailKind,
        payload: serde_json::Value,
    ) -> ReasoningDetail {
        ReasoningDetail {
            kind,
            id: None,
            format: format.map(str::to_string),
            index: None,
            payload,
        }
    }

    // -------------------------------------------------------------------
    // Finding 5: lift_reasoning_details skips non-openai-responses-v1
    // entries and aggregates the dropped formats for debug logging.
    // -------------------------------------------------------------------

    #[test]
    fn lift_skips_anthropic_format_details() {
        // Arrange: one detail with anthropic-claude-v1 format.
        let details = vec![make_detail(
            Some("anthropic-claude-v1"),
            ReasoningDetailKind::Text,
            json!({"text": "some reasoning"}),
        )];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert: no items emitted for anthropic-format details.
        assert!(
            out.is_empty(),
            "expected no Reasoning items from anthropic-claude-v1 details"
        );
    }

    #[test]
    fn lift_skips_format_less_details() {
        // Arrange: detail with no format tag.
        let details = vec![make_detail(
            None,
            ReasoningDetailKind::Text,
            json!({"text": "some reasoning"}),
        )];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert: no items emitted for format-less details.
        assert!(
            out.is_empty(),
            "expected no Reasoning items from format-less details"
        );
    }

    #[test]
    fn lift_includes_openai_responses_v1_details() {
        // Arrange: a v1 detail carrying an encrypted_content signature
        // (the only shape that can be validly replayed).
        let details = vec![make_detail(
            Some(OPENAI_RESPONSES_FORMAT),
            ReasoningDetailKind::Encrypted,
            json!({"encrypted_content": "SIG"}),
        )];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert: openai-responses-v1 details produce a Reasoning item.
        assert_eq!(
            out.len(),
            1,
            "expected one Reasoning item from openai-responses-v1 detail"
        );
    }

    /// An unrecognized kind has no slot in this egress's `reasoning`
    /// item -- summary/content/encrypted_content are the complete set --
    /// so it must be dropped, the same silent no-op every other arm in
    /// `lift_reasoning_details` already applies when its own expected
    /// payload field is absent. Paired with the recognized-kind test
    /// above as the positive control.
    #[test]
    fn lift_skips_unrecognized_kind_detail() {
        // Arrange
        let details = vec![make_detail(
            Some(OPENAI_RESPONSES_FORMAT),
            ReasoningDetailKind::Other("future.kind".to_string()),
            json!({"text": "some future payload"}),
        )];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert
        assert!(
            out.is_empty(),
            "expected no Reasoning items from an unrecognized-kind detail"
        );
    }

    #[test]
    fn lift_skips_v1_detail_with_empty_encrypted_content() {
        // Arrange: a v1 detail with no encrypted_content (text only).
        // It cannot be validly replayed, so it must be dropped to avoid
        // a dangling reasoning id on the wire.
        let details = vec![make_detail(
            Some(OPENAI_RESPONSES_FORMAT),
            ReasoningDetailKind::Text,
            json!({"text": "the reasoning text"}),
        )];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert
        assert!(
            out.is_empty(),
            "expected no Reasoning items for a v1 detail with empty encrypted_content"
        );
    }

    #[test]
    fn skip_path_never_logs_the_reasoning_item_id() {
        // The empty-encrypted_content skip path drops a reasoning item
        // that carries an upstream id. That id is a reasoning-replay
        // artifact and must never reach a log line at any level -- capture
        // every emitted event and assert the id appears in neither the
        // message nor any field value.
        const SECRET_ID: &str = "rs_SECRET_ITEM_ID_DO_NOT_LOG";
        let details = vec![ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some(SECRET_ID.to_string()),
            format: Some(OPENAI_RESPONSES_FORMAT.to_string()),
            index: None,
            payload: json!({"text": "the reasoning text"}),
        }];

        let events = routectl_testkit::capture_events(|| {
            let mut out = Vec::new();
            lift(&details, AuthKind::ChatgptOauth, &mut out);
            assert!(out.is_empty(), "empty-encrypted item must be skipped");
        });

        for ev in &events {
            assert!(
                !ev.message.contains(SECRET_ID),
                "reasoning item id leaked into a log message: {:?}",
                ev.message
            );
            for (name, value) in &ev.fields {
                assert!(
                    !value.contains(SECRET_ID),
                    "reasoning item id leaked into log field {name}={value}"
                );
            }
        }
    }

    #[test]
    fn lift_replays_v1_detail_with_non_empty_encrypted_content() {
        // Arrange: a v1 detail that carries both text and a signature.
        let details = vec![
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Text,
                json!({"text": "the reasoning text"}),
            ),
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Encrypted,
                json!({"encrypted_content": "SIG"}),
            ),
        ];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert: a single Reasoning item carrying the signature.
        assert_eq!(out.len(), 1, "expected one replayed Reasoning item");
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = &out[0]
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert_eq!(encrypted_content, "SIG");
    }

    #[test]
    fn lift_mixed_formats_only_includes_v1() {
        // Arrange: mix of openai-responses-v1 and anthropic-claude-v1.
        // The v1 entries carry a signature so the v1 item is replayable.
        let details = vec![
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Text,
                json!({"text": "openai reasoning"}),
            ),
            make_detail(
                Some(OPENAI_RESPONSES_FORMAT),
                ReasoningDetailKind::Encrypted,
                json!({"encrypted_content": "SIG"}),
            ),
            make_detail(
                Some("anthropic-claude-v1"),
                ReasoningDetailKind::Text,
                json!({"text": "anthropic reasoning"}),
            ),
        ];

        // Act
        let mut out = Vec::new();
        lift(&details, AuthKind::ChatgptOauth, &mut out);

        // Assert: only the v1 item is included.
        assert_eq!(
            out.len(),
            1,
            "expected exactly one item (the openai-responses-v1 detail)"
        );
    }

    /// Split a `Debug`-rendered slice-of-String field value into its
    /// element strings so the sample's length and contents can be
    /// asserted without pinning the whole rendering byte-for-byte.
    fn debug_list_entries(rendered: &str) -> Vec<String> {
        let inner = rendered
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        if inner.is_empty() {
            return Vec::new();
        }
        inner.split(", ").map(|e| e.trim().to_string()).collect()
    }

    /// Run the lift over `details` and return the aggregated
    /// foreign-format DEBUG record.
    fn format_skip_event(details: &[ReasoningDetail]) -> routectl_testkit::CapturedEvent {
        let events = routectl_testkit::capture_events(|| {
            let mut out = Vec::new();
            lift(details, AuthKind::ChatgptOauth, &mut out);
        });
        let matches: Vec<_> = events
            .iter()
            .filter(|e| {
                e.message.contains(
                    "openai-responses: skipped reasoning_details entries with a non-Responses-family format",
                )
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "exactly one aggregated format record expected; got events: {events:?}"
        );
        matches[0].clone()
    }

    fn foreign_format_detail(format: Option<&str>) -> ReasoningDetail {
        make_detail(
            format,
            ReasoningDetailKind::Text,
            json!({"text": "some reasoning"}),
        )
    }

    /// More DISTINCT foreign formats than the log cap must leave the
    /// rendered sample at the cap with `formats_truncated` set, while
    /// `skipped` stays the exact total.
    #[test]
    fn lift_caps_distinct_skipped_formats_and_flags_truncation() {
        // Arrange: 12 details over 9 distinct foreign formats.
        let mut details: Vec<_> = (0..9)
            .map(|i| foreign_format_detail(Some(&format!("foreign-format-{i}"))))
            .collect();
        details.extend((0..3).map(|i| foreign_format_detail(Some(&format!("foreign-format-{i}")))));

        // Act
        let event = format_skip_event(&details);

        // Assert
        assert_eq!(event.field("skipped"), Some("12"));
        assert_eq!(
            event.field("formats_truncated"),
            Some("true"),
            "a rejected distinct format must flag the sample as truncated"
        );
        let entries = debug_list_entries(event.field("formats").expect("formats field present"));
        assert_eq!(entries.len(), MAX_LOGGED_DIAGNOSTIC_ITEMS);
    }

    /// The defect this guards: deriving the truncation flag from
    /// offered-vs-stored counts. Many details sharing one format store a
    /// single entry that represents them all, so nothing was dropped.
    #[test]
    fn lift_reports_no_truncation_when_one_skipped_format_repeats() {
        // Arrange
        let details: Vec<_> = (0..10)
            .map(|_| foreign_format_detail(Some("anthropic-claude-v1")))
            .collect();

        // Act
        let event = format_skip_event(&details);

        // Assert
        assert_eq!(event.field("skipped"), Some("10"));
        assert_eq!(
            event.field("formats_truncated"),
            Some("false"),
            "repeats of a stored format drop nothing, so the sample is whole"
        );
        let entries = debug_list_entries(event.field("formats").expect("formats field present"));
        assert_eq!(entries, vec!["\"anthropic-claude-v1\""]);
    }

    /// A caller-supplied tag reaches the record sanitized: no raw control
    /// character, length-capped, and tags differing only in control
    /// characters share one slot. An absent tag keeps its placeholder.
    #[test]
    fn lift_sanitizes_skipped_format_tags_and_keeps_the_absent_placeholder() {
        // Arrange
        let long_tag = "z".repeat(1000);
        let details = vec![
            foreign_format_detail(Some("evil\nformat\r\0tag")),
            foreign_format_detail(Some("evil\rformat\n\0tag")),
            foreign_format_detail(Some(&long_tag)),
            foreign_format_detail(None),
        ];

        // Act
        let event = format_skip_event(&details);

        // Assert
        let rendered = event.field("formats").expect("formats field present");
        for raw in ['\n', '\r', '\0'] {
            assert!(
                !rendered.contains(raw),
                "a raw control character reached the log field: {rendered:?}"
            );
        }
        assert!(
            !rendered.contains(&long_tag),
            "an oversized tag must be length-capped; got: {rendered}"
        );
        let entries = debug_list_entries(rendered);
        assert_eq!(
            entries.len(),
            3,
            "tags differing only in control chars must share one slot; got: {entries:?}"
        );
        assert!(
            entries.contains(&"\"<none>\"".to_string()),
            "an absent format must render as a placeholder; got: {entries:?}"
        );
    }

    // -------------------------------------------------------------------
    // Finding 6: translate_thinking_part gates signature on format.
    // -------------------------------------------------------------------

    #[test]
    fn anthropic_format_thinking_does_not_leak_signature_into_encrypted_content() {
        // Arrange: ContentPart::Thinking path passes format = None
        // (KnownContentPart::Thinking carries no format field).
        let thinking = "I reasoned carefully about this.";
        let signature = Some("anthropic_sig_MUST_NOT_APPEAR");

        // Act
        let item = translate_thinking_part(thinking, signature, None, AuthKind::ChatgptOauth);

        // Assert: the signature is not replayable on this lane, so no item
        // is produced at all -- there is nothing for it to leak into.
        assert!(
            item.is_none(),
            "an unreplayable signature must produce no reasoning item"
        );
    }

    #[test]
    fn openai_responses_format_forwards_signature_to_encrypted_content() {
        // Arrange: openai-responses-v1 path (came from reasoning_details).
        let thinking = "Some intermediate reasoning";
        let signature = Some("test-openai-sig-not-real");

        // Act
        let item = translate_thinking_part(
            thinking,
            signature,
            Some(OPENAI_RESPONSES_FORMAT),
            AuthKind::ChatgptOauth,
        );

        // Assert: the signature IS forwarded.
        let Some(ResponseInputItem::Reasoning {
            encrypted_content, ..
        }) = item
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        assert_eq!(
            encrypted_content, "test-openai-sig-not-real",
            "openai-responses-v1 signature must be forwarded as encrypted_content"
        );
    }

    #[test]
    fn openai_responses_format_no_signature_emits_no_item() {
        // Arrange: openai-responses-v1 format, no signature available.
        let thinking = "Some reasoning";

        // Act
        let item = translate_thinking_part(
            thinking,
            None,
            Some(OPENAI_RESPONSES_FORMAT),
            AuthKind::ChatgptOauth,
        );

        // Assert: nothing to replay -> no item, rather than an item whose
        // upstream id would dangle.
        assert!(
            item.is_none(),
            "a signature-less thinking block must produce no reasoning item"
        );
    }

    // -------------------------------------------------------------------
    // Egress replay gate: family recognition + per-lane carry/strip.
    // -------------------------------------------------------------------

    fn signed_detail(format: &str) -> ReasoningDetail {
        make_detail(
            Some(format),
            ReasoningDetailKind::Encrypted,
            json!({"encrypted_content": "SIG"}),
        )
    }

    fn lifted_count(format: &str, lane: AuthKind) -> usize {
        let mut out = Vec::new();
        lift(&[signed_detail(format)], lane, &mut out);
        out.len()
    }

    #[test]
    fn lift_carries_a_detail_toward_a_lane_of_the_same_proven_family() {
        assert_eq!(
            lifted_count(CODEX_OAUTH, AuthKind::ChatgptOauth),
            1,
            "codex-lane detail must replay onto a codex lane"
        );
        assert_eq!(
            lifted_count(OPENAI_APIKEY, AuthKind::ApiKey),
            1,
            "api-key-lane detail must replay onto an api-key lane"
        );
        assert_eq!(
            lifted_count(CODEX_OAUTH, AuthKind::ApiKey),
            1,
            "the two first-party lanes share one validator family"
        );
        assert_eq!(
            lifted_count(BEDROCK_MANTLE, AuthKind::BedrockMantle),
            1,
            "mantle-lane detail must replay onto a mantle lane"
        );
    }

    #[test]
    fn lift_strips_a_detail_toward_a_lane_of_a_different_proven_family() {
        assert_eq!(
            lifted_count(CODEX_OAUTH, AuthKind::BedrockMantle),
            0,
            "codex-lane detail is proven-incompatible with a mantle lane"
        );
        assert_eq!(
            lifted_count(BEDROCK_MANTLE, AuthKind::ChatgptOauth),
            0,
            "mantle-lane detail is proven-incompatible with a codex lane"
        );
    }

    #[test]
    fn lift_carries_a_gray_zone_detail_optimistically() {
        // The compatibility tag names no lane, so no pair involving it is
        // proven either way. Carrying it once is how the pair gets
        // settled; the provider neither retries nor learns.
        assert_eq!(
            lifted_count(OPENAI_RESPONSES_V1, AuthKind::BedrockMantle),
            1,
            "a gray-zone detail must be carried once, not stripped"
        );
        assert_eq!(
            lifted_count(OPENAI_RESPONSES_V1, AuthKind::ChatgptOauth),
            1,
            "a gray-zone detail must be carried once, not stripped"
        );
    }

    fn thinking_signature(format: Option<&str>, lane: AuthKind) -> Option<String> {
        let item = translate_thinking_part("reasoned", Some("SIG"), format, lane)?;
        let ResponseInputItem::Reasoning {
            encrypted_content, ..
        } = item
        else {
            panic!("expected ResponseInputItem::Reasoning");
        };
        Some(encrypted_content)
    }

    #[test]
    fn thinking_part_forwards_the_signature_for_every_recognized_lane_tag() {
        assert_eq!(
            thinking_signature(Some(CODEX_OAUTH), AuthKind::ChatgptOauth).as_deref(),
            Some("SIG")
        );
        assert_eq!(
            thinking_signature(Some(OPENAI_APIKEY), AuthKind::ApiKey).as_deref(),
            Some("SIG")
        );
        assert_eq!(
            thinking_signature(Some(BEDROCK_MANTLE), AuthKind::BedrockMantle).as_deref(),
            Some("SIG")
        );
    }

    #[test]
    fn thinking_part_drops_the_signature_across_proven_families() {
        assert!(
            thinking_signature(Some(CODEX_OAUTH), AuthKind::BedrockMantle).is_none(),
            "a codex-lane signature must not reach a mantle lane"
        );
        assert!(
            thinking_signature(Some(BEDROCK_MANTLE), AuthKind::ChatgptOauth).is_none(),
            "a mantle-lane signature must not reach a codex lane"
        );
    }

    #[test]
    fn thinking_part_forwards_a_gray_zone_signature_optimistically() {
        assert_eq!(
            thinking_signature(Some(OPENAI_RESPONSES_V1), AuthKind::BedrockMantle).as_deref(),
            Some("SIG")
        );
    }
}

#[cfg(test)]
mod tool_calls_field_tests {
    use serde_json::json;

    use routectl_core::{ContentPart, KnownContentPart, Message, MessageContent, Role};

    use super::super::AuthKind;
    use super::super::types::ResponseInputItem;
    use super::build_input;

    fn user_text(text: &str) -> Message {
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

    /// An assistant turn whose tool call rides ONLY on the OpenAI-shape
    /// `tool_calls` field (content null/empty, no ToolUse content part)
    /// must emit a `function_call` input item carrying the call_id, name,
    /// and forwarded arguments string -- so the following
    /// `function_call_output` is not dangling.
    #[test]
    fn assistant_openai_tool_calls_field_emits_function_call_item() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("sunny".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("call_1".into()),
                tool_calls: None,
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert: a function_call item carries the call data.
        let fc_idx = out
            .iter()
            .position(|i| {
                matches!(
                    i,
                    ResponseInputItem::FunctionCall { call_id, name, arguments }
                        if call_id == "call_1"
                            && name == "get_weather"
                            && arguments == "{\"city\":\"SF\"}"
                )
            })
            .expect("tool_calls field must produce a matching function_call item");

        // The function_call_output references the same id and follows
        // the function_call (not orphaned).
        let fco_idx = out
            .iter()
            .position(|i| {
                matches!(i, ResponseInputItem::FunctionCallOutput { call_id, .. } if call_id == "call_1")
            })
            .expect("function_call_output must be present");
        assert!(
            fc_idx < fco_idx,
            "function_call must precede its function_call_output"
        );
    }

    /// A tool call with a missing id is synthesized to a non-empty
    /// call_id so the Responses upstream does not reject an empty id.
    #[test]
    fn assistant_tool_call_missing_id_is_synthesized_on_responses() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert
        let call_id = out
            .iter()
            .find_map(|i| match i {
                ResponseInputItem::FunctionCall { call_id, .. } => Some(call_id),
                _ => None,
            })
            .expect("missing-id tool call must still produce a function_call item");
        assert!(
            !call_id.is_empty(),
            "missing id must be synthesized non-empty, got empty"
        );
    }

    /// When the assistant turn ALREADY carries a ToolUse content part,
    /// setting `tool_calls` as well must NOT double-emit the function_call
    /// item (the content-part walk already emitted it).
    #[test]
    fn assistant_tool_use_content_part_not_doubled_by_tool_calls_field() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: "call_1".into(),
                        name: "get_weather".into(),
                        input: json!({"city": "SF"}),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"SF\"}"},
                })]),
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert: exactly one function_call item, not two.
        let count = out
            .iter()
            .filter(|i| matches!(i, ResponseInputItem::FunctionCall { .. }))
            .count();
        assert_eq!(
            count, 1,
            "function_call must not be doubled when both content part and tool_calls are set"
        );
    }

    /// An id that REQUIRES escaping must reach the wire as the SAME value
    /// on the `function_call` and on the `function_call_output` that
    /// answers it. The Responses API correlates output to call by
    /// `call_id`, so a sanitized call plus a raw output silently loses the
    /// tool result: the model answers without ever seeing it.
    ///
    /// This variant exercises the canonical `Role::Tool` output path.
    #[test]
    fn escaping_id_correlates_call_and_output_via_tool_role_message() {
        // Arrange
        let raw = "call.foo:1";
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": raw,
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("sunny".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some(raw.into()),
                tool_calls: None,
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert
        assert_eq!(
            emitted_call_id(&out),
            emitted_output_call_id(&out),
            "function_call and function_call_output must carry the same call_id"
        );
    }

    /// Same correlation invariant on the Anthropic-shape lane: the tool
    /// result arrives as a `ToolResult` content part on a USER turn and is
    /// lifted by `extract_tool_results`, while the call arrives as a
    /// `ToolUse` content part on the assistant turn.
    #[test]
    fn escaping_id_correlates_call_and_output_via_tool_result_part() {
        // Arrange
        let raw = "call.foo:1";
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: raw.into(),
                        name: "get_weather".into(),
                        input: json!({}),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolResult {
                        tool_use_id: raw.into(),
                        content: json!("sunny"),
                        is_error: None,
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert
        assert_eq!(
            emitted_call_id(&out),
            emitted_output_call_id(&out),
            "function_call and function_call_output must carry the same call_id"
        );
    }

    /// The two ingress shapes for one logical call must land on the same
    /// wire id: an `id` arriving as a `ToolUse` content part and the same
    /// `id` arriving on the OpenAI-shape `tool_calls` field.
    #[test]
    fn escaping_id_agrees_across_both_call_emit_shapes() {
        // Arrange
        let raw = "call.foo:1";
        let via_part = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: raw.into(),
                        name: "f".into(),
                        input: json!({}),
                        cache_control: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];
        let via_field = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": raw,
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
        ];

        // Act
        let part_out = build_input("test", AuthKind::ChatgptOauth, &via_part).unwrap();
        let field_out = build_input("test", AuthKind::ChatgptOauth, &via_field).unwrap();

        // Assert
        assert_eq!(
            emitted_call_id(&part_out),
            emitted_call_id(&field_out),
            "one logical id must reach the wire identically from either ingress shape"
        );
    }

    /// A wire-safe id is untouched by the correlation fix -- the common
    /// case must not be mangled.
    #[test]
    fn wire_safe_id_passes_through_unchanged_on_both_sides() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Null,
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_abc-1",
                    "type": "function",
                    "function": {"name": "f", "arguments": "{}"},
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("call_abc-1".into()),
                tool_calls: None,
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert
        assert_eq!(emitted_call_id(&out), "call_abc-1");
        assert_eq!(emitted_output_call_id(&out), "call_abc-1");
    }

    fn emitted_call_id(items: &[ResponseInputItem]) -> &str {
        items
            .iter()
            .find_map(|i| match i {
                ResponseInputItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .expect("a function_call item must be emitted")
    }

    fn emitted_output_call_id(items: &[ResponseInputItem]) -> &str {
        items
            .iter()
            .find_map(|i| match i {
                ResponseInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .expect("a function_call_output item must be emitted")
    }

    /// A single-turn assistant text message with no tool_calls produces a
    /// single assistant Message item and no function_call items.
    #[test]
    fn assistant_plain_text_turn_unchanged_without_tool_calls() {
        // Arrange
        let messages = vec![
            user_text("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("just text".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();

        // Assert: no function_call items; the assistant Message survives.
        assert!(
            out.iter()
                .all(|i| !matches!(i, ResponseInputItem::FunctionCall { .. })),
            "no function_call items expected on a plain text turn"
        );
        let assistant_msg = out
            .iter()
            .find(|i| matches!(i, ResponseInputItem::Message { role, .. } if role == "assistant"));
        assert!(assistant_msg.is_some(), "assistant Message must survive");
    }
}

#[cfg(test)]
mod role_other_field_tests {
    use routectl_core::{Message, MessageContent, Role};

    use super::super::AuthKind;
    use super::super::types::ResponseInputItem;
    use super::build_input;

    fn user_text(text: &str) -> Message {
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

    fn other_text(tag: &str, text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::Other(tag.to_string()),
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// The FIDELITY-faithful move for Responses (unlike the other three
    /// egresses): the wire role field is a plain string, so an
    /// unrecognized role forwards VERBATIM rather than collapsing to
    /// "user" -- plus exactly one DEBUG naming the tag.
    #[test]
    fn unrecognized_role_forwards_verbatim_with_debug() {
        // Arrange
        let messages = vec![other_text("narrator", "hello there")];

        // Act
        let mut out = Vec::new();
        let events = routectl_testkit::capture_events(|| {
            out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();
        });

        // Assert
        assert_eq!(out.len(), 1, "the turn must survive translation");
        assert!(
            matches!(&out[0], ResponseInputItem::Message { role, .. } if role == "narrator"),
            "must forward the original tag verbatim, got: {:?}",
            out[0]
        );
        let debug_events: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::DEBUG && e.field("role") == Some("narrator"))
            .collect();
        assert_eq!(
            debug_events.len(),
            1,
            "exactly one DEBUG must name the unrecognized role tag, got: {events:?}"
        );
    }

    /// Sibling positive control: a recognized `Role::User` turn takes the
    /// ordinary path and emits no such DEBUG, proving the assertion above
    /// actually exercises the `Role::Other` arm rather than firing
    /// regardless of role.
    #[test]
    fn known_user_role_emits_no_unrecognized_role_debug() {
        // Arrange
        let messages = vec![user_text("hello there")];

        // Act
        let mut out = Vec::new();
        let events = routectl_testkit::capture_events(|| {
            out = build_input("test", AuthKind::ChatgptOauth, &messages).unwrap();
        });

        // Assert
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], ResponseInputItem::Message { role, .. } if role == "user"));
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("unrecognized message role")),
            "a recognized role must not trip the unrecognized-role fallback, got: {events:?}"
        );
    }
}

/// Both ingress shapes for a tool output must treat an unusable
/// correlating id the same way: fail the request, never drop the block.
/// A `call_id` is the only thing that binds an output to its call, so a
/// dropped output means the model silently answers without the tool
/// result.
#[cfg(test)]
mod empty_tool_id_policy_tests {
    use serde_json::json;

    use routectl_core::{ContentPart, KnownContentPart, Message, MessageContent, Role};

    use super::super::AuthKind;
    use super::super::types::ResponseInputItem;
    use super::build_input;

    fn user_text(text: &str) -> Message {
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

    /// A `Role::Tool` message carrying `tool_call_id`, which may be
    /// absent (`None`) or present-but-empty.
    fn tool_role_message(tool_call_id: Option<&str>) -> Message {
        Message {
            refusal: None,
            role: Role::Tool,
            content: MessageContent::Text("sunny".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: tool_call_id.map(str::to_string),
            tool_calls: None,
        }
    }

    /// A user turn whose content is a single Anthropic-shape
    /// `ToolResult` part. The canonical `tool_use_id` is a plain
    /// `String`, so an absent id is only representable as an empty one.
    fn tool_result_turn(tool_use_id: &str) -> Message {
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(
                KnownContentPart::ToolResult {
                    tool_use_id: tool_use_id.into(),
                    content: json!("sunny"),
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

    fn output_call_ids(out: &[ResponseInputItem]) -> Vec<&str> {
        out.iter()
            .filter_map(|i| match i {
                ResponseInputItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn empty_tool_call_id_on_tool_role_message_errors() {
        // Arrange
        let messages = vec![user_text("hi"), tool_role_message(Some(""))];

        // Act
        let result = build_input("test", AuthKind::ChatgptOauth, &messages);

        // Assert
        let err = result.expect_err("an empty tool_call_id must fail the request");
        assert!(
            err.to_string().contains("tool_call_id"),
            "error must name the offending field, got: {err}"
        );
    }

    #[test]
    fn missing_tool_call_id_on_tool_role_message_errors() {
        // Arrange
        let messages = vec![user_text("hi"), tool_role_message(None)];

        // Act
        let result = build_input("test", AuthKind::ChatgptOauth, &messages);

        // Assert
        let err = result.expect_err("an absent tool_call_id must fail the request");
        assert!(
            err.to_string().contains("tool_call_id"),
            "error must name the offending field, got: {err}"
        );
    }

    #[test]
    fn empty_tool_use_id_on_tool_result_part_errors() {
        // Arrange
        let messages = vec![user_text("hi"), tool_result_turn("")];

        // Act
        let result = build_input("test", AuthKind::ChatgptOauth, &messages);

        // Assert
        let err = result.expect_err("an empty tool_use_id must fail the request");
        assert!(
            err.to_string().contains("tool_use_id"),
            "error must name the offending field, got: {err}"
        );
    }

    #[test]
    fn non_empty_tool_call_id_on_tool_role_message_emits_the_output() {
        // Arrange
        let messages = vec![user_text("hi"), tool_role_message(Some("call_1"))];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages)
            .expect("a valid tool_call_id must translate");

        // Assert
        assert_eq!(output_call_ids(&out), vec!["call_1"]);
    }

    #[test]
    fn non_empty_tool_use_id_on_tool_result_part_emits_the_output() {
        // Arrange
        let messages = vec![user_text("hi"), tool_result_turn("call_1")];

        // Act
        let out = build_input("test", AuthKind::ChatgptOauth, &messages)
            .expect("a valid tool_use_id must translate");

        // Assert
        assert_eq!(output_call_ids(&out), vec!["call_1"]);
    }
}

/// `ContentPart::Other.type_tag` is the verbatim wire `type` string with no
/// charset validation on the ingress path, and both drop-warn arms render it
/// into a `part_type` tracing field. A raw `\n` there would forge a whole log
/// line; a raw ANSI CSI sequence would let a caller scroll an operator's
/// terminal. Both arms must therefore emit only printable ASCII.
#[cfg(test)]
mod part_type_log_injection_tests {
    use routectl_core::{ContentPart, Message, MessageContent, Role};

    use super::super::AuthKind;
    use super::build_input;

    /// A `type_tag` carrying every injection primitive at once: a newline
    /// (forges a log line), a carriage return (rewrites the current one), and
    /// an ANSI CSI erase-display sequence (scrolls prior output away).
    const HOSTILE_TYPE_TAG: &str = "video_url\nforged=1\r\x1b[2Jgone";

    fn hostile_part() -> ContentPart {
        ContentPart::Other {
            type_tag: HOSTILE_TYPE_TAG.into(),
            cache_control: None,
            extras: serde_json::Map::new(),
        }
    }

    fn msg_with_hostile_part(role: Role) -> Message {
        Message {
            refusal: None,
            role,
            content: MessageContent::Parts(vec![hostile_part()]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// Every `part_type` field emitted while normalizing `role`'s hostile
    /// part, as rendered by the subscriber.
    fn rendered_part_types(role: Role) -> Vec<String> {
        let messages = vec![msg_with_hostile_part(role)];
        let events = routectl_testkit::capture_events(|| {
            build_input("test", AuthKind::ChatgptOauth, &messages)
                .expect("a forward-compat part warn-drops rather than failing");
        });
        events
            .iter()
            .filter_map(|e| e.field("part_type"))
            .map(str::to_string)
            .collect()
    }

    fn assert_no_raw_control_chars(rendered: &[String], role: &str) {
        assert!(
            !rendered.is_empty(),
            "the {role} forward-compat arm must emit a part_type field"
        );
        for value in rendered {
            assert!(
                !value.chars().any(|c| c.is_control()),
                "{role} part_type must carry no raw control char; got {value:?}"
            );
            for forbidden in ['\n', '\r', '\u{1b}'] {
                assert!(
                    !value.contains(forbidden),
                    "{role} part_type must not carry {forbidden:?}; got {value:?}"
                );
            }
            assert!(
                value.starts_with("video_url"),
                "{role} part_type must keep its printable prefix; got {value:?}"
            );
        }
    }

    #[test]
    fn user_forward_compat_part_type_is_control_char_free_in_logs() {
        // Arrange + Act
        let rendered = rendered_part_types(Role::User);

        // Assert
        assert_no_raw_control_chars(&rendered, "user");
    }

    #[test]
    fn assistant_forward_compat_part_type_is_control_char_free_in_logs() {
        // Arrange + Act
        let rendered = rendered_part_types(Role::Assistant);

        // Assert
        assert_no_raw_control_chars(&rendered, "assistant");
    }
}
