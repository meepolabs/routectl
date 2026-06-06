//! Canonical `req.system` -> Anthropic wire `system` translation.
//!
//! Two surfaces: `translate_system` maps a typed `SystemContent`
//! (Text or per-block) onto `AnthropicSystem`, preserving per-block
//! cache_control and citations; `lift_legacy_system` is the
//! backwards-compat fallback that lifts `Role::System` messages into a
//! flat `AnthropicSystem::Text` for direct callers that bypass an
//! ingress. Both are `pub(crate)` so the Bedrock Converse egress can
//! reuse the canonical-side mapping (single source of truth).

use routectl_core::{Message, MessageContent, Role, SystemContent};

use super::types::{AnthropicSystem, AnthropicSystemBlock};

/// Convert canonical `SystemContent` to wire `AnthropicSystem`. Preserves
/// per-block cache_control and citations.
pub(crate) fn translate_system(s: &SystemContent) -> AnthropicSystem {
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

/// Backwards-compat fallback: lift Role::System messages out of the
/// messages array into a flat AnthropicSystem::Text. Used only when
/// `req.system` is None. Returns None when no System messages are
/// present, or when all System messages contain only non-text content
/// (Parts without text blocks, Null) -- avoids emitting a meaningless
/// `system: ""` upstream and the extra newlines from joining blanks.
///
/// `pub(crate)` so the Bedrock Converse egress can reuse the same
/// legacy-shape fallback (single source of truth).
pub(crate) fn lift_legacy_system(messages: &[Message]) -> Option<AnthropicSystem> {
    let texts: Vec<String> = messages
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
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(AnthropicSystem::Text(texts.join("\n")))
    }
}
