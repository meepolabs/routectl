//! Per-block `prompt_cache_breakpoint` capture for the Responses ingress.
//!
//! The canonical content part has no home for a breakpoint, so the ingress
//! records each one beside the request instead: where the block sits in the
//! text-block stream the egress will rebuild, a digest of its text, the
//! value verbatim, and where the client put it. Canonical content is never
//! touched.

use routectl_core::{
    ContentPart, KnownContentPart, Message, MessageContent, ResponsesCacheBreakpoint, Role,
};
use routectl_providers::openai_responses::breakpoint_text_digest;
use serde_json::Value;

const BREAKPOINT_KEY: &str = "prompt_cache_breakpoint";

/// A breakpoint found in the client's body, before the system-message lift
/// decides which messages stay in `messages[]`.
pub(super) struct BreakpointSite {
    /// `input[]` index of the item holding the block.
    item: usize,
    /// Position of the block among its item's non-empty text blocks; `None`
    /// when the block is not one.
    text_rank: Option<usize>,
    text_digest: String,
    value: Value,
    client_pointer: String,
}

/// Every breakpoint on a content block of a modeled `input[]` item: a
/// message's `content` and a `function_call_output`'s `output`. Unmodeled
/// items are forwarded verbatim, so their breakpoints need no record.
pub(super) fn collect_sites(input: &Value) -> Vec<BreakpointSite> {
    let Some(items) = input.as_array() else {
        return Vec::new();
    };
    let mut sites = Vec::new();
    for (item, value) in items.iter().enumerate() {
        if let Some((field, blocks)) = block_array(value) {
            collect_item_sites(item, field, blocks, &mut sites);
        }
    }
    sites
}

/// Resolve each site's ordinal against the post-lift `messages`.
/// `message_items` maps each PRE-lift message to its `input[]` index and
/// `lifted` lists the pre-lift indices the lift removed.
pub(super) fn resolve(
    sites: Vec<BreakpointSite>,
    messages: &[Message],
    message_items: &[usize],
    lifted: &[usize],
) -> Vec<ResponsesCacheBreakpoint> {
    if sites.is_empty() {
        return Vec::new();
    }
    let spans = ordinal_spans(messages, message_items, lifted);
    sites
        .into_iter()
        .map(|site| {
            let ordinal = spans
                .iter()
                .find(|span| span.item == site.item)
                .zip(site.text_rank)
                .and_then(|(span, rank)| (rank < span.count).then_some(span.base + rank));
            ResponsesCacheBreakpoint {
                ordinal,
                text_digest: site.text_digest,
                value: site.value,
                client_pointer: site.client_pointer,
            }
        })
        .collect()
}

/// The ordinals one post-lift message occupies in the text-block stream.
struct OrdinalSpan {
    item: usize,
    base: usize,
    count: usize,
}

fn ordinal_spans(
    messages: &[Message],
    message_items: &[usize],
    lifted: &[usize],
) -> Vec<OrdinalSpan> {
    let kept_items = message_items
        .iter()
        .enumerate()
        .filter(|(message, _)| !lifted.contains(message))
        .map(|(_, &item)| item);
    let mut spans = Vec::new();
    let mut next = 0usize;
    for (message, item) in messages.iter().zip(kept_items) {
        if !carries_breakpoints(&message.role) {
            continue;
        }
        let count = text_block_count(&message.content);
        spans.push(OrdinalSpan {
            item,
            base: next,
            count,
        });
        next += count;
    }
    spans
}

const fn carries_breakpoints(role: &Role) -> bool {
    matches!(role, Role::User | Role::System | Role::Other(_))
}

fn text_block_count(content: &MessageContent) -> usize {
    match content {
        MessageContent::Text(text) => usize::from(!text.is_empty()),
        MessageContent::Parts(parts) => parts
            .iter()
            .filter(|part| {
                matches!(part, ContentPart::Known(KnownContentPart::Text { text, .. }) if !text.is_empty())
            })
            .count(),
        MessageContent::Null => 0,
    }
}

/// The block array of a modeled item, with the field it sits under. The
/// item-kind test matches the one `build_messages_from_items` applies.
fn block_array(item: &Value) -> Option<(&'static str, &[Value])> {
    let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
    let field = match kind {
        "message" => "content",
        "" if item.get("role").is_some() => "content",
        "function_call_output" => "output",
        _ => return None,
    };
    let blocks = item.get(field).and_then(Value::as_array)?;
    Some((field, blocks.as_slice()))
}

fn collect_item_sites(item: usize, field: &str, blocks: &[Value], sites: &mut Vec<BreakpointSite>) {
    let mut text_blocks = 0usize;
    for (index, block) in blocks.iter().enumerate() {
        let text = block_text(block);
        let text_rank = text.filter(|t| !t.is_empty()).map(|_| {
            text_blocks += 1;
            text_blocks - 1
        });
        let Some(value) = block.get(BREAKPOINT_KEY) else {
            continue;
        };
        sites.push(BreakpointSite {
            item,
            text_rank,
            text_digest: breakpoint_text_digest(text.unwrap_or_default()),
            value: value.clone(),
            client_pointer: format!("/input/{item}/{field}/{index}"),
        });
    }
}

/// The text a block contributes to canonical content, read the way
/// `parse_content_block` reads it; `None` for a non-text block.
fn block_text(block: &Value) -> Option<&str> {
    match block.get("type").and_then(Value::as_str) {
        Some("input_text" | "output_text") => Some(
            block
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

#[cfg(test)]
#[path = "breakpoints_tests.rs"]
mod tests;
