//! Inbound normalization of `reasoning_details` payload spellings.
//!
//! [`ReasoningDetailKind`] accepts the Anthropic block names (`thinking`,
//! `redacted_thinking`) as serde aliases onto the canonical variants they
//! already mean. The discriminator is only half the vocabulary, though:
//! an Anthropic `thinking` block spells its text under the key `thinking`,
//! while every canonical reader looks under `text`. Accepting the
//! discriminator without moving the payload key would deserialize the
//! detail and then silently drop its content at every egress.
//!
//! This normalization is shared by both ingresses on purpose. The defect
//! it closes was exactly a divergence -- one ingress emitting a
//! vocabulary the other rejected -- and two independently maintained
//! copies would re-open it.

use std::sync::Arc;

use crate::schema::{ChatRequest, Message, ReasoningDetailKind};

/// Rewrite Anthropic-spelled reasoning payload keys into the canonical
/// spelling every egress reads.
///
/// Currently one rewrite: a `Text` detail carrying its content under
/// `thinking` (the Anthropic block's key) gets it moved to `text`. An
/// explicit `text` already present wins -- a client sending both is
/// taken at its canonical word rather than having it overwritten.
///
/// `Encrypted` needs no rewrite: `encrypted_detail_data` and the
/// Anthropic egress already read the Anthropic `data` spelling directly.
///
/// Scans read-only first and touches `Arc::make_mut` only when a rewrite
/// is actually due, so the overwhelmingly common no-Anthropic-vocabulary
/// request does not pay a message-buffer copy against the CoW seam
/// documented on [`ChatRequest::messages`].
pub fn normalize_reasoning_detail_payloads(req: &mut ChatRequest) {
    if !req.messages.iter().any(message_needs_rewrite) {
        return;
    }
    for msg in Arc::make_mut(&mut req.messages) {
        for detail in &mut msg.reasoning_details {
            if !detail_needs_rewrite(detail) {
                continue;
            }
            let Some(obj) = detail.payload.as_object_mut() else {
                continue;
            };
            if let Some(thinking) = obj.remove("thinking") {
                obj.insert("text".into(), thinking);
            }
        }
    }
}

fn message_needs_rewrite(msg: &Message) -> bool {
    msg.reasoning_details.iter().any(detail_needs_rewrite)
}

fn detail_needs_rewrite(detail: &crate::schema::ReasoningDetail) -> bool {
    matches!(detail.kind, ReasoningDetailKind::Text)
        && detail
            .payload
            .as_object()
            .is_some_and(|o| o.contains_key("thinking") && !o.contains_key("text"))
}

#[cfg(test)]
#[path = "reasoning_ingest_tests.rs"]
mod tests;
