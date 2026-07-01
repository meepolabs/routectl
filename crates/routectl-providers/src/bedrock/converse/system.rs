//! Canonical `req.system` -> Converse `system: [...]` translation.
//!
//! AWS models the system surface as an array of single-key blocks --
//! `[{text}]` for prompt text and `[..., {cachePoint}]` interleaves for
//! breakpoints. Empty system collapses to None (AWS rejects
//! `system: [{text:""}]` with "minimum length of 1").

use routectl_core::ChatRequest;

use crate::anthropic_api::request::{lift_legacy_system, translate_system};
use crate::anthropic_api::types::AnthropicSystem;

use super::types::{CachePoint, ConverseSystemBlock};

/// Translate the canonical `system` field into AWS's
/// `Vec<ConverseSystemBlock>`. Reuses
/// `crate::anthropic_api::request::translate_system` to keep typed
/// SystemContent::Blocks parsing in one place; the Anthropic shape's
/// per-block `cache_control` becomes a sibling `{cachePoint}` entry
/// here (Bedrock cache_point semantics).
///
/// When `req.system` is None, falls back to lifting Role::System
/// messages out of `req.messages` via `lift_legacy_system` -- mirrors
/// the Anthropic egress so direct callers (no ingress, just
/// `messages: [{role:"system",...}]`) don't silently lose their system
/// prompt.
pub(super) fn build_system(req: &ChatRequest) -> Option<Vec<ConverseSystemBlock>> {
    // Drop the Claude Code billing/attribution block before translation:
    // Bedrock is a third-party upstream and must not receive the client
    // fingerprint the block carries.
    let mut billing_dropped = false;
    let filtered_system = req
        .system
        .as_ref()
        .and_then(|s| crate::system_filter::strip_billing_attribution(s, &mut billing_dropped));
    if billing_dropped {
        tracing::warn!(
            provider = %req.model,
            "bedrock-converse egress: Claude Code billing/attribution system block dropped",
        );
    }
    let anthropic_system = filtered_system
        .as_ref()
        .map(translate_system)
        .or_else(|| lift_legacy_system(&req.messages))?;
    let mut out: Vec<ConverseSystemBlock> = Vec::new();
    match anthropic_system {
        AnthropicSystem::Text(t) if t.is_empty() => {
            // Empty system is just absent -- avoid emitting a stray
            // `system: [{text: ""}]` which AWS rejects with
            // "minimum length of 1".
        }
        AnthropicSystem::Text(t) => out.push(ConverseSystemBlock::Text(t)),
        AnthropicSystem::Blocks(blocks) => {
            for b in &blocks {
                // An empty-text system block can't anchor a cachePoint: AWS
                // rejects a `{cachePoint}` with no preceding content block.
                // Skip the whole entry so a verbatim-preserved empty block
                // with cache_control doesn't emit a leading/orphan marker.
                if b.text.is_empty() {
                    continue;
                }
                out.push(ConverseSystemBlock::Text(b.text.clone()));
                if let Some(cc) = b.cache_control.as_ref() {
                    out.push(ConverseSystemBlock::CachePoint(
                        CachePoint::default_with_ttl(Some(cc.effective_ttl().to_string())),
                    ));
                }
            }
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{CacheControl, ChatRequest, SystemBlock, SystemContent};

    fn block(text: &str, cc: Option<CacheControl>) -> SystemBlock {
        SystemBlock {
            kind: "text".into(),
            text: text.into(),
            cache_control: cc,
            citations: None,
        }
    }

    fn req_with_system(system: SystemContent) -> ChatRequest {
        ChatRequest {
            system: Some(system),
            ..Default::default()
        }
    }

    /// An empty-text system block carrying cache_control (preserved verbatim
    /// by the Anthropic ingress) must NOT emit a leading/orphan cachePoint --
    /// AWS rejects a `{cachePoint}` with no preceding content block.
    #[test]
    fn empty_text_block_with_cache_control_emits_no_orphan_cachepoint() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "",
            Some(CacheControl::ephemeral_5m()),
        )]));

        // Act
        let out = build_system(&req);

        // Assert
        assert!(
            out.is_none(),
            "an empty-text block must produce neither Text nor an orphan \
             cachePoint, got: {out:?}"
        );
    }

    /// A non-empty system block with cache_control emits its Text block
    /// followed by the anchored cachePoint -- regression guard that the
    /// orphan fix didn't drop legitimate breakpoints.
    #[test]
    fn non_empty_block_with_cache_control_emits_text_then_cachepoint() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "be helpful",
            Some(CacheControl::ephemeral_5m()),
        )]));

        // Act
        let out = build_system(&req).expect("non-empty system must produce blocks");

        // Assert
        assert_eq!(out.len(), 2, "expected a Text block then a cachePoint");
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "be helpful"),
            "first block must be the system text, got: {:?}",
            out[0]
        );
        assert!(
            matches!(&out[1], ConverseSystemBlock::CachePoint(_)),
            "cachePoint must follow the anchoring text block, got: {:?}",
            out[1]
        );
    }

    /// A leading empty block (with marker) followed by a real block must
    /// drop only the empty entry; the real block and its cachePoint survive
    /// and no orphan cachePoint leads the array.
    #[test]
    fn empty_block_before_real_block_drops_only_the_empty_entry() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![
            block("", Some(CacheControl::ephemeral_5m())),
            block("real prompt", Some(CacheControl::ephemeral_1h())),
        ]));

        // Act
        let out = build_system(&req).expect("the real block must survive");

        // Assert
        assert_eq!(out.len(), 2, "only the real block + its cachePoint remain");
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "real prompt"),
            "the surviving array must start with the real text, got: {:?}",
            out[0]
        );
        assert!(matches!(&out[1], ConverseSystemBlock::CachePoint(_)));
    }

    /// The Claude Code billing/attribution block must be dropped before
    /// the Converse translation; a normal sibling block survives.
    #[test]
    fn billing_block_dropped_keeps_normal_block() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![
            block("x-anthropic-billing-header: v=1; fp=secret", None),
            block("you are helpful", None),
        ]));

        // Act
        let out = build_system(&req).expect("the normal block must survive");

        // Assert
        assert_eq!(out.len(), 1, "only the normal block survives");
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "you are helpful"),
            "billing block must be dropped, got: {:?}",
            out[0]
        );
    }

    /// A mid-string occurrence of the prefix is a normal prompt and survives.
    #[test]
    fn mid_string_billing_prefix_is_preserved() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![block(
            "intro x-anthropic-billing-header: not at start",
            None,
        )]));

        // Act
        let out = build_system(&req).expect("a mid-string block must survive");

        // Assert
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "intro x-anthropic-billing-header: not at start"),
            "a mid-string occurrence must NOT be treated as the billing block, got: {:?}",
            out[0]
        );
    }

    /// Leading whitespace before the prefix still matches; the block is dropped.
    #[test]
    fn billing_block_with_leading_whitespace_is_dropped() {
        // Arrange
        let req = req_with_system(SystemContent::Blocks(vec![
            block("  \n\tx-anthropic-billing-header: v=1", None),
            block("real prompt", None),
        ]));

        // Act
        let out = build_system(&req).expect("the real block must survive");

        // Assert
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "real prompt"),
            "leading-whitespace billing block must still be dropped, got: {:?}",
            out[0]
        );
    }
}
