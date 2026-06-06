//! Canonical `req.system` -> Responses `instructions` translation.
//!
//! The Responses API has no first-class system role: the prior chat-
//! completions `system` message is collapsed into a top-level
//! `instructions` string. When canonical carries
//! `SystemContent::Blocks`, we flatten each block's text joined by
//! `"\n\n"` (one blank line between blocks) so block boundaries remain
//! visible to the model but the field stays a flat string.
//!
//! Lossy seam: per-block `cache_control` markers cannot ride the
//! Responses wire (no Anthropic-style prompt cache surface here yet),
//! so we drop at DEBUG level. The caller's request_id will be on the span
//! emitted by the Provider's `complete()` instrumentation, so the
//! debug event is correlated automatically.

use routectl_core::{ChatRequest, SystemContent};

/// Build the `instructions` field for the Responses API from the
/// canonical `system` field. Returns `None` when no system content is
/// present so the caller can skip the field entirely (the parent
/// `ResponsesRequest` always serializes `instructions`, even when
/// empty; an empty string `""` is accepted by the server as
/// "no system prompt").
pub(super) fn translate_system(req: &ChatRequest) -> Option<String> {
    let s = req.system.as_ref()?;
    match s {
        SystemContent::Text(t) if t.is_empty() => None,
        SystemContent::Text(t) => Some(t.clone()),
        SystemContent::Blocks(blocks) => {
            warn_on_cache_control_loss(blocks);
            let combined: Vec<String> = blocks
                .iter()
                .filter(|b| !b.text.is_empty())
                .map(|b| b.text.clone())
                .collect();
            if combined.is_empty() {
                None
            } else {
                Some(combined.join("\n\n"))
            }
        }
    }
}

/// Emit a debug event for each block carrying a `cache_control` marker that
/// will be dropped on the Responses wire. Operators can raise the log level
/// to DEBUG to see the loss and can either move the prompt to an
/// Anthropic-shape provider or accept the drop.
fn warn_on_cache_control_loss(blocks: &[routectl_core::SystemBlock]) {
    let dropped = blocks.iter().filter(|b| b.cache_control.is_some()).count();
    if dropped > 0 {
        tracing::debug!(
            dropped_count = dropped,
            "openai-responses: dropping cache_control on system block(s); \
             Responses API has no prompt-cache breakpoint surface yet"
        );
    }
}
