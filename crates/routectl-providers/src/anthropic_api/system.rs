//! Canonical `req.system` -> Anthropic wire `system` translation.
//!
//! Two surfaces: `translate_system` maps a typed `SystemContent`
//! (Text or per-block) onto `AnthropicSystem`, preserving per-block
//! cache_control and citations; `lift_legacy_system_stripped` is the
//! backwards-compat fallback that lifts `Role::System` messages into a
//! flat system text for direct callers that bypass an ingress, dropping the
//! Claude Code billing/attribution block per text part before joining so
//! the fingerprint never reaches a third-party host via the lift. With a
//! canonical system present the anthropic-api egress forwards the
//! `Role::System` turns in place (see `messages::SystemTurnPolicy`); a
//! lane that cannot forward them lifts the turns an ingress kept in place
//! (`kept_system_turns`) and appends them after it (`append_system`).
//! All are `pub(crate)` so the Bedrock Converse egress can reuse the
//! canonical-side mapping (single source of truth).

use routectl_core::{Message, Role, SystemContent};

use super::types::{AnthropicSystem, AnthropicSystemBlock};

use crate::system_filter::system_role_texts_stripped;

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

/// Fallback for a request with no canonical `req.system`: lifts the text of
/// the Role::System messages into one flat system text, dropping every text
/// part that is a Claude Code billing/attribution block BEFORE joining. The
/// strip must run per PART -- once joined, a billing block that follows
/// legitimate text (in another part or another message) is no longer at the
/// leading position the predicate tests. Blank parts are dropped and the
/// survivors join with "\n", one message's parts and the messages alike.
/// Sets `dropped = true` when at least one block was removed so the caller
/// can emit a single contents-free WARN. Returns None when nothing survives
/// the strip (the system collapses to absent).
///
/// On anthropic-api the both-present case forwards the turns in place, and
/// the forwarding walk runs the same billing predicate per block.
pub fn lift_legacy_system_stripped(
    messages: &[Message],
    dropped: &mut bool,
) -> Option<SystemContent> {
    let kept: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .filter_map(|m| {
            let parts = system_role_texts_stripped(std::slice::from_ref(m), dropped);
            (!parts.is_empty()).then(|| parts.join("\n"))
        })
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(SystemContent::Text(kept.join("\n")))
    }
}

/// The `Role::System` turns carrying a non-text part. An ingress keeps such a
/// turn in place instead of hoisting its text into the canonical system, so
/// a lane that cannot ship the turn lifts its text from here.
pub fn kept_system_turns(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| matches!(m.role, Role::System) && m.content.has_non_text_part())
        .cloned()
        .collect()
}

/// The wire system: the canonical system, then the lifted text after it. With
/// both present the result is block-shaped, the canonical blocks first.
pub fn append_system(
    primary: Option<AnthropicSystem>,
    lifted: Option<AnthropicSystem>,
) -> Option<AnthropicSystem> {
    match (primary, lifted) {
        (Some(primary), Some(lifted)) => {
            let mut blocks = system_blocks(primary);
            blocks.extend(system_blocks(lifted));
            Some(AnthropicSystem::Blocks(blocks))
        }
        (primary, lifted) => primary.or(lifted),
    }
}

fn system_blocks(system: AnthropicSystem) -> Vec<AnthropicSystemBlock> {
    match system {
        AnthropicSystem::Text(text) => vec![AnthropicSystemBlock {
            kind: "text".into(),
            text,
            cache_control: None,
            citations: None,
        }],
        AnthropicSystem::Blocks(blocks) => blocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::MessageContent;

    const BILLING: &str = "x-anthropic-billing-header: v=1; fp=abc";

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
        assert!(
            matches!(&out, Some(SystemContent::Text(t)) if t == "you are helpful"),
            "{out:?}"
        );
    }

    fn sys_parts_msg(texts: &[&str]) -> Message {
        Message {
            content: MessageContent::Parts(
                texts
                    .iter()
                    .map(|t| {
                        routectl_core::ContentPart::Known(routectl_core::KnownContentPart::Text {
                            text: (*t).into(),
                            citations: None,
                            cache_control: None,
                        })
                    })
                    .collect(),
            ),
            ..sys_msg("")
        }
    }

    #[test]
    fn stripped_withholds_a_billing_part_in_either_position_of_one_message() {
        let rows: [(&str, Vec<&str>); 2] = [
            ("billing part last", vec!["you are helpful", BILLING]),
            ("billing part first", vec![BILLING, "you are helpful"]),
        ];
        for (name, parts) in rows {
            let mut dropped = false;

            let out = lift_legacy_system_stripped(&[sys_parts_msg(&parts)], &mut dropped);

            assert!(dropped, "{name}");
            assert!(
                matches!(&out, Some(SystemContent::Text(t)) if t == "you are helpful"),
                "{name}: {out:?}"
            );
        }
    }

    #[test]
    fn stripped_fingerprint_free_parts_join_as_before() {
        let msgs = vec![
            sys_parts_msg(&["one", "  ", "two"]),
            sys_msg("three"),
            sys_parts_msg(&[" "]),
        ];
        let mut dropped = false;

        let out = lift_legacy_system_stripped(&msgs, &mut dropped);

        assert!(!dropped);
        assert!(
            matches!(&out, Some(SystemContent::Text(t)) if t == "one\ntwo\nthree"),
            "{out:?}"
        );
    }

    #[test]
    fn a_billing_part_in_a_lifted_system_message_never_reaches_the_anthropic_wire() {
        let req = routectl_core::ChatRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![
                sys_parts_msg(&["you are helpful", BILLING]),
                Message {
                    role: Role::User,
                    ..sys_msg("hello")
                },
            ]
            .into(),
            ..Default::default()
        };

        let wire = crate::anthropic_api::request::normalize("test", &req, false, None, false, true)
            .expect("normalize must succeed");

        assert_eq!(wire["system"], "you are helpful", "{wire}");
        assert!(
            !wire.to_string().contains("x-anthropic-billing-header:"),
            "{wire}"
        );
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
}
