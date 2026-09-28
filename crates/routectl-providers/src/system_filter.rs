//! Shared filter for the Claude Code billing/attribution system block.
//!
//! The real Claude Code client injects an in-band `system` text block
//! whose text begins with `x-anthropic-billing-header:` (carrying its
//! version + a client fingerprint). routectl must not forward that
//! fingerprint to any upstream that isn't the genuine Anthropic billing
//! party, so every egress strips it before flatten/translation. This
//! module provides the predicate that identifies the block, a helper
//! that drops it from a canonical `SystemContent`, and one that collects
//! `Role::System` message text with it withheld.
//!
//! Each egress collects and projects system content in its own wire shape
//! and records every withhold into its per-request
//! `ClientFingerprintStripTally`. The anthropic-api egress strips
//! unconditionally too: an anthropic-api provider can be pointed at a
//! third-party host, where the OAuth-gated identity cloak does not fire, so
//! the strip has to run on the always-on normalize path rather than inside
//! the cloak.

use routectl_core::{SystemBlock, SystemContent};

/// Prefix (after trimming leading whitespace) that marks a Claude Code
/// billing/attribution system block.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// True when `text` is a Claude Code billing/attribution block: after
/// trimming leading whitespace it starts with `x-anthropic-billing-header:`.
/// A mid-string occurrence does NOT match -- only the leading position.
pub fn is_billing_attribution_block(text: &str) -> bool {
    text.trim_start().starts_with(BILLING_PREFIX)
}

/// Return a copy of `system` with any Claude Code billing/attribution block
/// removed. For `Blocks`, drops matching entries (preserving the rest and
/// their order). For `Text`, returns `None` when the whole string is the
/// billing block (the system collapses to absent). Returns `None` when the
/// filtered result carries no content. `dropped` is set to `true` when at
/// least one block was removed, so callers can emit a single contents-free
/// log line.
pub fn strip_billing_attribution(
    system: &SystemContent,
    dropped: &mut bool,
) -> Option<SystemContent> {
    match system {
        SystemContent::Text(s) => {
            if is_billing_attribution_block(s) {
                *dropped = true;
                None
            } else {
                Some(SystemContent::Text(s.clone()))
            }
        }
        SystemContent::Blocks(blocks) => {
            let kept: Vec<SystemBlock> = blocks
                .iter()
                .filter(|b| !is_billing_attribution_block(&b.text))
                .cloned()
                .collect();
            if kept.len() != blocks.len() {
                *dropped = true;
            }
            if kept.is_empty() {
                None
            } else {
                Some(SystemContent::Blocks(kept))
            }
        }
    }
}

/// The text of every `Role::System` message, in message order and, within a
/// multi-part message, in part order -- one entry per non-blank text part,
/// the same granularity an ingress gives each part when it hoists system
/// messages into `system` blocks. Non-text parts carry no system text.
///
/// Every entry the billing/attribution predicate matches is withheld and
/// `withheld` set. The predicate runs per PART, before any caller joins them:
/// a fingerprint part that follows legitimate text in the same message is not
/// at the leading position the predicate tests once the two are joined.
#[cfg(any(feature = "openai-responses", feature = "gemini"))]
pub fn system_role_texts_stripped(
    messages: &[routectl_core::Message],
    withheld: &mut bool,
) -> Vec<String> {
    use routectl_core::{ContentPart, KnownContentPart, MessageContent, Role};

    let mut texts = Vec::new();
    for message in messages.iter().filter(|m| matches!(m.role, Role::System)) {
        let parts: Vec<&str> = match &message.content {
            MessageContent::Text(text) => vec![text.as_str()],
            MessageContent::Parts(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Known(KnownContentPart::Text { text, .. }) => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
            MessageContent::Null => Vec::new(),
        };
        for text in parts.into_iter().filter(|t| !t.trim().is_empty()) {
            if is_billing_attribution_block(text) {
                *withheld = true;
            } else {
                texts.push(text.to_string());
            }
        }
    }
    texts
}

/// `messages` with the billing/attribution block withheld from every
/// `Role::System` message, or `None` when no message carries it, so the caller
/// keeps its own copy untouched.
///
/// For an egress that keeps system-role messages in place rather than lifting
/// their text out. The predicate runs per text PART, for the same reason as
/// [`system_role_texts_stripped`]. Every other message and every surviving
/// part keeps its position; a system message left with no content is removed
/// rather than shipped empty. Equal content is never treated as a duplicate.
#[cfg(feature = "openai-compat")]
pub fn strip_system_role_messages(
    messages: &[routectl_core::Message],
) -> Option<Vec<routectl_core::Message>> {
    if !messages.iter().any(system_message_carries_billing_block) {
        return None;
    }
    Some(
        messages
            .iter()
            .filter_map(without_billing_attribution)
            .collect(),
    )
}

#[cfg(feature = "openai-compat")]
fn system_message_carries_billing_block(message: &routectl_core::Message) -> bool {
    use routectl_core::{MessageContent, Role};

    matches!(message.role, Role::System)
        && match &message.content {
            MessageContent::Text(text) => is_billing_attribution_block(text),
            MessageContent::Parts(parts) => parts.iter().any(is_billing_text_part),
            MessageContent::Null => false,
        }
}

#[cfg(feature = "openai-compat")]
fn is_billing_text_part(part: &routectl_core::ContentPart) -> bool {
    use routectl_core::{ContentPart, KnownContentPart};

    matches!(
        part,
        ContentPart::Known(KnownContentPart::Text { text, .. })
            if is_billing_attribution_block(text)
    )
}

/// `message` with the block withheld, or `None` when nothing is left of it.
#[cfg(feature = "openai-compat")]
fn without_billing_attribution(message: &routectl_core::Message) -> Option<routectl_core::Message> {
    use routectl_core::{Message, MessageContent};

    if !system_message_carries_billing_block(message) {
        return Some(message.clone());
    }
    match &message.content {
        MessageContent::Parts(parts) => {
            let kept: Vec<_> = parts
                .iter()
                .filter(|part| !is_billing_text_part(part))
                .cloned()
                .collect();
            (!kept.is_empty()).then(|| Message {
                content: MessageContent::Parts(kept),
                ..message.clone()
            })
        }
        MessageContent::Text(_) | MessageContent::Null => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(text: &str) -> SystemBlock {
        SystemBlock {
            kind: "text".into(),
            text: text.into(),
            cache_control: None,
            citations: None,
        }
    }

    #[test]
    fn predicate_matches_exact_prefix() {
        assert!(is_billing_attribution_block(
            "x-anthropic-billing-header: v=1; fp=abc"
        ));
    }

    #[test]
    fn predicate_matches_leading_whitespace_before_prefix() {
        assert!(is_billing_attribution_block(
            "  \n\tx-anthropic-billing-header: v=1"
        ));
    }

    #[test]
    fn predicate_does_not_match_mid_string_prefix() {
        assert!(!is_billing_attribution_block(
            "be helpful x-anthropic-billing-header: v=1"
        ));
    }

    #[test]
    fn predicate_does_not_match_normal_prompt() {
        assert!(!is_billing_attribution_block("you are a helpful assistant"));
    }

    #[test]
    fn strip_blocks_drops_only_the_billing_block() {
        // Arrange
        let system = SystemContent::Blocks(vec![
            block("x-anthropic-billing-header: v=1; fp=secret"),
            block("you are helpful"),
        ]);
        let mut dropped = false;

        // Act
        let out = strip_billing_attribution(&system, &mut dropped);

        // Assert
        assert!(dropped);
        match out.expect("normal block must survive") {
            SystemContent::Blocks(b) => {
                assert_eq!(b.len(), 1);
                assert_eq!(b[0].text, "you are helpful");
            }
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn strip_blocks_preserves_mid_string_match() {
        // Arrange
        let system = SystemContent::Blocks(vec![block(
            "intro x-anthropic-billing-header: not at start",
        )]);
        let mut dropped = false;

        // Act
        let out = strip_billing_attribution(&system, &mut dropped);

        // Assert
        assert!(!dropped);
        match out.expect("mid-string block must survive") {
            SystemContent::Blocks(b) => assert_eq!(b.len(), 1),
            other => panic!("expected Blocks, got {other:?}"),
        }
    }

    #[test]
    fn strip_text_drops_whole_billing_string() {
        // Arrange
        let system = SystemContent::Text("x-anthropic-billing-header: v=1".into());
        let mut dropped = false;

        // Act
        let out = strip_billing_attribution(&system, &mut dropped);

        // Assert
        assert!(dropped);
        assert!(
            out.is_none(),
            "a pure billing Text system collapses to None"
        );
    }

    #[test]
    fn strip_text_preserves_normal_string() {
        // Arrange
        let system = SystemContent::Text("you are helpful".into());
        let mut dropped = false;

        // Act
        let out = strip_billing_attribution(&system, &mut dropped);

        // Assert
        assert!(!dropped);
        assert!(matches!(out, Some(SystemContent::Text(s)) if s == "you are helpful"));
    }
}
