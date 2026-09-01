//! Canonical `req.system` -> Converse `system: [...]` translation.
//!
//! AWS models the system surface as an array of single-key blocks --
//! `[{text}]` for prompt text and `[..., {cachePoint}]` interleaves for
//! breakpoints. Empty system collapses to None (AWS rejects
//! `system: [{text:""}]` with "minimum length of 1").

use routectl_core::ChatRequest;

use crate::anthropic_api::request::{lift_legacy_system_stripped, translate_system};
use crate::anthropic_api::types::{AnthropicSystem, AnthropicSystemBlock};

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
/// prompt. When BOTH a top-level `system` and Role::System messages are
/// present, `merge_system_sources` combines them into one `system`
/// array rather than picking one and discarding the other -- see its
/// doc comment for why merge, and in what order.
pub(super) fn build_system(req: &ChatRequest) -> Option<Vec<ConverseSystemBlock>> {
    // Drop the Claude Code billing/attribution block before translation:
    // Bedrock is a third-party upstream and must not receive the client
    // fingerprint the block carries.
    let mut billing_dropped = false;
    let filtered_system = req
        .system
        .as_ref()
        // A blank canonical system reads as "no canonical system supplied"
        // (same as None), so it falls through to the Role::System lift
        // rather than suppressing it. Without this, a whitespace-only
        // `system` would ship as `[{text: "   "}]`: accepted by AWS
        // (non-zero length) but a meaningless instruction.
        .filter(|s| !s.is_blank())
        .and_then(|s| crate::system_filter::strip_billing_attribution(s, &mut billing_dropped));
    if billing_dropped {
        tracing::warn!(
            model = %routectl_core::sanitize_for_log(&req.model),
            "bedrock-converse egress: Claude Code billing/attribution system block dropped",
        );
    }
    let primary = filtered_system.as_ref().map(translate_system);
    // The lifted content runs through the SAME billing-attribution strip as
    // the top-level system. Before both sources were merged this lift only
    // ran when no top-level system existed, so an unfiltered lift could not
    // pair a fingerprint with other system content; merging removed that
    // guarantee, and this upstream is third-party either way.
    let mut legacy_billing_dropped = false;
    let legacy = lift_legacy_system_stripped(&req.messages, &mut legacy_billing_dropped)
        .as_ref()
        .map(translate_system);
    if legacy_billing_dropped {
        tracing::warn!(
            model = %routectl_core::sanitize_for_log(&req.model),
            "bedrock-converse egress: Claude Code billing/attribution block \
             dropped from a system-role message",
        );
    }
    let anthropic_system = merge_system_sources(primary, legacy)?;
    let mut out: Vec<ConverseSystemBlock> = Vec::new();
    match anthropic_system {
        AnthropicSystem::Text(t) if t.trim().is_empty() => {
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
                if b.text.trim().is_empty() {
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

/// Combine the top-level canonical `system` translation with the
/// legacy Role::System message lift. Lane: bedrock-converse,
/// construction-time translation.
///
/// Converse's wire `system` field is an ordered array of blocks -- the
/// same shape a canonical `SystemContent::Blocks` produces -- so both
/// sources fit without any structural conflict. This is unlike the
/// `messages[]` array, which Converse restricts to `user`/`assistant`
/// roles and therefore has no slot to forward a Role::System turn in
/// place the way the Anthropic-API egress does when both are present.
/// A request naming both a top-level `system` and Role::System messages
/// carries two distinct system inputs, and the wire shape has room for
/// both, so nothing here needs to be picked over the other and
/// discarded.
///
/// ORDER: the top-level `system` field's blocks come first, the
/// legacy-lifted Role::System text is appended after. The top-level
/// field is the canonical, structured input; the message-array shape
/// is the backwards-compat path for direct callers with no ingress.
/// Ordering the canonical input first matches the precedence a caller
/// supplying it would expect, while still delivering the legacy
/// content to the model instead of discarding it.
fn merge_system_sources(
    primary: Option<AnthropicSystem>,
    legacy: Option<AnthropicSystem>,
) -> Option<AnthropicSystem> {
    match (primary, legacy) {
        (Some(p), Some(l)) => {
            tracing::debug!(
                "bedrock-converse egress: merging top-level system with Role::System \
                 message content; both were present on the same request"
            );
            let mut merged = as_system_blocks(p);
            merged.extend(as_system_blocks(l));
            Some(AnthropicSystem::Blocks(merged))
        }
        (Some(p), None) => Some(p),
        (None, Some(l)) => Some(l),
        (None, None) => None,
    }
}

/// Normalize an `AnthropicSystem` to its block-array form so
/// `merge_system_sources` can concatenate two sources regardless of
/// which variant each one is. A `Text` variant becomes a single block
/// with no cache_control or citations -- the same information a plain
/// string system carries on the wire.
fn as_system_blocks(s: AnthropicSystem) -> Vec<AnthropicSystemBlock> {
    match s {
        AnthropicSystem::Text(t) => vec![AnthropicSystemBlock {
            kind: "text".into(),
            text: t,
            cache_control: None,
            citations: None,
        }],
        AnthropicSystem::Blocks(b) => b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{
        CacheControl, ChatRequest, Message, MessageContent, Role, SystemBlock, SystemContent,
    };
    use routectl_testkit::capture_events;

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

    /// Pin: a blank canonical system (empty string, whitespace-only, or
    /// blocks whose every text is blank) produces no `system` array at all.
    /// AWS rejects `system: [{text:""}]` outright; whitespace-only would be
    /// accepted but is a meaningless instruction.
    #[test]
    fn blank_canonical_system_produces_no_system_array() {
        for system in [
            SystemContent::Text(String::new()),
            SystemContent::Text("   \n\t ".into()),
            SystemContent::Blocks(vec![block("", None), block("  \n", None)]),
        ] {
            // Arrange
            let req = req_with_system(system);

            // Act
            let out = build_system(&req);

            // Assert
            assert!(
                out.is_none(),
                "a blank canonical system must produce no system array, got: {out:?}"
            );
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

    /// The headline defect this fixes: a request carrying BOTH a
    /// top-level `system` and a Role::System message must not discard
    /// either. Converse's `system` array has room for both blocks, so
    /// both must survive, top-level first, legacy-lifted content
    /// appended after.
    #[test]
    fn both_top_level_and_role_system_message_are_merged_not_dropped() {
        // Arrange
        let req = ChatRequest {
            system: Some(SystemContent::Text("top-level prompt".into())),
            messages: vec![sys_msg("legacy prompt")].into(),
            ..Default::default()
        };

        // Act
        let mut out = None;
        let events = capture_events(|| {
            out = build_system(&req);
        });
        let out = out.expect("both system sources present must produce a system array");

        // Assert: both texts survive, top-level first.
        assert_eq!(out.len(), 2, "expected both system texts, got: {out:?}");
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "top-level prompt"),
            "the top-level system must come first, got: {:?}",
            out[0]
        );
        assert!(
            matches!(&out[1], ConverseSystemBlock::Text(t) if t == "legacy prompt"),
            "the legacy-lifted Role::System text must be appended, got: {:?}",
            out[1]
        );
        assert!(
            events.iter().any(|e| e.level == tracing::Level::DEBUG
                && e.message.contains("merging top-level system")),
            "the merge must stay observable through a real tracing event; got: {events:?}"
        );
    }

    /// Positive control: with only a top-level `system` present (no
    /// Role::System messages), no merge happens and no merge log fires --
    /// pins that the merge path is genuinely conditional on both sources
    /// being present, proving the negative-control assertion above would
    /// actually fail if the merge log fired unconditionally.
    #[test]
    fn only_top_level_system_present_emits_no_merge_log() {
        // Arrange
        let req = req_with_system(SystemContent::Text("solo top-level".into()));

        // Act
        let mut out = None;
        let events = capture_events(|| {
            out = build_system(&req);
        });
        let out = out.expect("a top-level system alone must still produce a system array");

        // Assert
        assert_eq!(out.len(), 1);
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("merging top-level system")),
            "no merge should be logged when only one system source is present: {events:?}"
        );
    }

    /// Positive control: with only a Role::System message present (no
    /// top-level `system`), the legacy lift still runs alone and no merge
    /// log fires -- mirrors `only_top_level_system_present_emits_no_merge_log`
    /// for the other single-source shape.
    #[test]
    fn only_legacy_role_system_message_present_emits_no_merge_log() {
        // Arrange
        let req = ChatRequest {
            messages: vec![sys_msg("solo legacy")].into(),
            ..Default::default()
        };

        // Act
        let mut out = None;
        let events = capture_events(|| {
            out = build_system(&req);
        });
        let out = out.expect("a legacy system message alone must still produce a system array");

        // Assert
        assert_eq!(out.len(), 1);
        assert!(
            matches!(&out[0], ConverseSystemBlock::Text(t) if t == "solo legacy"),
            "the legacy-lifted text must survive, got: {:?}",
            out[0]
        );
        assert!(
            !events
                .iter()
                .any(|e| e.message.contains("merging top-level system")),
            "no merge should be logged when only one system source is present: {events:?}"
        );
    }

    /// The billing/attribution strip must reach BOTH system sources, not
    /// just the top-level one. This pairing is the gap the merge opened:
    /// before it, a system-role message was lifted only when no top-level
    /// system existed, so an unfiltered lift could never pair a client
    /// fingerprint with other system content. Merging removed that
    /// guarantee, and this upstream is third-party either way.
    #[test]
    fn a_billing_block_in_a_system_message_is_stripped_even_when_merging() {
        // Arrange -- both sources present, the fingerprint riding in the
        // message-array half.
        let req = ChatRequest {
            system: Some(SystemContent::Text("top-level prompt".into())),
            messages: vec![
                sys_msg("x-anthropic-billing-header: v=1; fp=abc"),
                sys_msg("legacy prompt"),
            ]
            .into(),
            ..Default::default()
        };

        // Act
        let mut out = None;
        let events = capture_events(|| {
            out = build_system(&req);
        });
        let out = out.expect("both system sources present must produce a system array");

        // Assert -- the fingerprint is gone, the real content on both sides
        // survives, and the strip is reported.
        let rendered = format!("{out:?}");
        assert!(
            !rendered.contains("x-anthropic-billing-header"),
            "the client fingerprint must not reach a third-party upstream, got: {rendered}"
        );
        assert!(
            rendered.contains("top-level prompt") && rendered.contains("legacy prompt"),
            "both real system texts must survive the strip, got: {rendered}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.level == tracing::Level::WARN
                    && e.message.contains("billing/attribution")),
            "dropping a fingerprint must be reported, got: {events:?}"
        );
    }
}
