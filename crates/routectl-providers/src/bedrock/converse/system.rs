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
    let anthropic_system = req
        .system
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
                if !b.text.is_empty() {
                    out.push(ConverseSystemBlock::Text(b.text.clone()));
                }
                if let Some(cc) = b.cache_control.as_ref() {
                    out.push(ConverseSystemBlock::CachePoint(
                        CachePoint::default_with_ttl(Some(cc.effective_ttl().to_string())),
                    ));
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}
