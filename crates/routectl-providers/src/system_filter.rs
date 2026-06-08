//! Shared filter for the Claude Code billing/attribution system block.
//!
//! The real Claude Code client injects an in-band `system` text block
//! whose text begins with `x-anthropic-billing-header:` (carrying its
//! version + a client fingerprint). On an Anthropic-ingress request that
//! egresses to a NON-Anthropic upstream (openai-compat, bedrock), routectl
//! would otherwise forward that fingerprint to a third party. This module
//! provides the predicate that identifies the block and a helper that drops
//! it from a canonical `SystemContent`, so each non-Anthropic egress can
//! strip it before flatten/translation.
//!
//! The anthropic-api egress does NOT use this filter: the block belongs to
//! Anthropic and is forwarded unchanged on the all-Anthropic path.

use routectl_core::{SystemBlock, SystemContent};

/// Prefix (after trimming leading whitespace) that marks a Claude Code
/// billing/attribution system block.
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// True when `text` is a Claude Code billing/attribution block: after
/// trimming leading whitespace it starts with `x-anthropic-billing-header:`.
/// A mid-string occurrence does NOT match -- only the leading position.
pub(crate) fn is_billing_attribution_block(text: &str) -> bool {
    text.trim_start().starts_with(BILLING_PREFIX)
}

/// Return a copy of `system` with any Claude Code billing/attribution block
/// removed. For `Blocks`, drops matching entries (preserving the rest and
/// their order). For `Text`, returns `None` when the whole string is the
/// billing block (the system collapses to absent). Returns `None` when the
/// filtered result carries no content. `dropped` is set to `true` when at
/// least one block was removed, so callers can emit a single contents-free
/// log line.
pub(crate) fn strip_billing_attribution(
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
