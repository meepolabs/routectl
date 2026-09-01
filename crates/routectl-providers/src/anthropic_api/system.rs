//! Canonical `req.system` -> Anthropic wire `system` translation.
//!
//! Two surfaces: `translate_system` maps a typed `SystemContent`
//! (Text or per-block) onto `AnthropicSystem`, preserving per-block
//! cache_control and citations; `lift_legacy_system` is the
//! backwards-compat fallback that lifts `Role::System` messages into a
//! flat `AnthropicSystem::Text` for direct callers that bypass an
//! ingress. Both lifts cover the `req.system`-ABSENT path only: with a
//! canonical system present the anthropic-api egress forwards the
//! `Role::System` turns in place (see `messages::SystemTurnPolicy`).
//! `lift_legacy_system_stripped` is the billing-aware variant
//! used by the anthropic-api egress: it drops the Claude Code
//! billing/attribution block per-message before joining so the
//! fingerprint never reaches a third-party host via the lift fallback.
//! All are `pub(crate)` so the Bedrock Converse egress can reuse the
//! canonical-side mapping (single source of truth).

use routectl_core::{Message, MessageContent, Role, SystemContent};

use super::types::{AnthropicSystem, AnthropicSystemBlock};

use crate::system_filter::is_billing_attribution_block;

/// Convert canonical `SystemContent` to wire `AnthropicSystem`. Preserves
/// per-block cache_control and citations.
///
/// Blank content is NOT filtered here: this is the pure typed mapping, and
/// callers decide what absent means for their wire. Each egress screens a
/// blank canonical `req.system` with `SystemContent::is_blank` before
/// calling, so `system: ""` never reaches an upstream.
pub fn translate_system(s: &SystemContent) -> AnthropicSystem {
    match s {
        SystemContent::Text(t) => AnthropicSystem::Text(t.clone()),
        SystemContent::Blocks(blocks) => AnthropicSystem::Blocks(
            blocks
                .iter()
                .map(|b| AnthropicSystemBlock {
                    kind: b.kind.clone(),
                    text: b.text.clone(),
                    cache_control: b.cache_control.clone(),
                    citations: b.citations.clone(),
                })
                .collect(),
        ),
    }
}

/// Collect the per-message system texts from the canonical `Role::System`
/// messages, preserving order. Drops empty/blank texts (and non-text Parts)
/// so a meaningless `system: ""` never lands upstream. One entry per
/// surviving System message -- the caller joins or filters them.
fn collect_system_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .filter_map(|m| match &m.content {
            MessageContent::Text(t) => {
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(t.clone())
                }
            }
            MessageContent::Parts(parts) => {
                // Pick out text content from typed parts. Image/Document/etc.
                // in a System message are not meaningful for the flat-text
                // lift and would have been dropped by the egress anyway.
                let collected: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match p {
                        routectl_core::ContentPart::Known(
                            routectl_core::KnownContentPart::Text { text, .. },
                        ) => {
                            let trimmed = text.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(text.clone())
                            }
                        }
                        _ => None,
                    })
                    .collect();
                if collected.is_empty() {
                    None
                } else {
                    Some(collected.join("\n"))
                }
            }
            MessageContent::Null => None,
        })
        .collect()
}

/// Backwards-compat fallback: lift Role::System messages out of the
/// messages array into a flat AnthropicSystem::Text. Used only when
/// `req.system` is None. Returns None when no System messages are
/// present, or when all System messages contain only non-text content
/// (Parts without text blocks, Null) -- avoids emitting a meaningless
/// `system: ""` upstream and the extra newlines from joining blanks.
///
/// This covers the `req.system`-ABSENT path only. When a canonical system
/// is present the anthropic-api egress forwards the Role::System turns in
/// place instead, so nothing lifts them.
///
/// `pub(crate)` so the Bedrock Converse egress can reuse the same
/// legacy-shape fallback (single source of truth). Gated on the
/// `bedrock` feature because the anthropic-api egress uses the
/// billing-aware `lift_legacy_system_stripped`.
///
/// TEST-ONLY. No egress calls this: the Converse path used to, back when
/// it lifted a system-role message only if no top-level system existed, so
/// a fingerprint could never ride alongside other system content. Once
/// that path began MERGING both sources the unfiltered lift became a way
/// to leak a client fingerprint to a third-party upstream, and Converse
/// moved to the stripped variant. Kept, compiled only under test, as the
/// contrast case its sibling's regression pin asserts against.
#[cfg(test)]
pub fn lift_legacy_system(messages: &[Message]) -> Option<AnthropicSystem> {
    let texts = collect_system_texts(messages);
    if texts.is_empty() {
        None
    } else {
        Some(AnthropicSystem::Text(texts.join("\n")))
    }
}

/// Billing-aware variant of `lift_legacy_system`: lifts Role::System
/// messages but drops any whose text is a Claude Code billing/attribution
/// block BEFORE joining. The strip must run per-message -- once joined, a
/// billing block fused with a real prompt no longer matches the
/// leading-prefix predicate. Sets `dropped = true` when at least one block
/// was removed so the caller can emit a single contents-free WARN. Returns
/// None when nothing survives the strip (the system collapses to absent).
///
/// Like the unfiltered lift, this covers the `req.system`-ABSENT path only;
/// the both-present case forwards the turns in place, and the forwarding
/// walk runs the same billing predicate per block.
pub fn lift_legacy_system_stripped(
    messages: &[Message],
    dropped: &mut bool,
) -> Option<SystemContent> {
    let mut kept: Vec<String> = Vec::new();
    for text in collect_system_texts(messages) {
        if is_billing_attribution_block(&text) {
            *dropped = true;
        } else {
            kept.push(text);
        }
    }
    if kept.is_empty() {
        None
    } else {
        Some(SystemContent::Text(kept.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sys_msg(text: &str) -> Message {
        Message {
            refusal: None,
            role: Role::System,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn stripped_drops_billing_keeps_normal() {
        let msgs = vec![
            sys_msg("x-anthropic-billing-header: v=1; fp=abc"),
            sys_msg("you are helpful"),
        ];
        let mut dropped = false;
        let out = lift_legacy_system_stripped(&msgs, &mut dropped);
        assert!(dropped);
        let text = match out.expect("non-billing prompt survives") {
            SystemContent::Text(t) => t,
            other => panic!("expected Text, got {other:?}"),
        };
        assert!(!text.contains("x-anthropic-billing-header:"));
        assert!(text.contains("you are helpful"));
    }

    #[test]
    fn stripped_pure_billing_collapses_to_none() {
        let msgs = vec![sys_msg("x-anthropic-billing-header: v=1; fp=abc")];
        let mut dropped = false;
        let out = lift_legacy_system_stripped(&msgs, &mut dropped);
        assert!(dropped);
        assert!(out.is_none());
    }

    #[test]
    fn stripped_no_system_messages_returns_none() {
        let msgs: Vec<Message> = vec![];
        let mut dropped = false;
        let out = lift_legacy_system_stripped(&msgs, &mut dropped);
        assert!(!dropped);
        assert!(out.is_none());
    }

    #[test]
    fn original_lift_still_includes_billing_text() {
        // Regression pin: the unfiltered lift_legacy_system keeps billing
        // text -- it is the caller's responsibility to strip when needed.
        // Bedrock-gated alongside the function it exercises.
        let msgs = vec![
            sys_msg("x-anthropic-billing-header: v=1; fp=abc"),
            sys_msg("you are helpful"),
        ];
        let out = lift_legacy_system(&msgs).expect("messages produce a system");
        match out {
            AnthropicSystem::Text(t) => {
                assert!(t.contains("x-anthropic-billing-header:"));
                assert!(t.contains("you are helpful"));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }
}
