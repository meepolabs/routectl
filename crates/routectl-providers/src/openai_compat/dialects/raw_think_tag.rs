//! Endpoints that emit `<think>...</think>` inline in the content
//! string (llama.cpp default for QwQ/DeepSeek when served raw).
//!
//! Request side is a passthrough. Response side runs a regex over
//! `msg.content` to lift the bracketed text into a typed
//! `reasoning_details` block. Streaming uses a different path
//! (`ThinkTagAccumulator` in `sse.rs`) because tags can span chunks,
//! so this dialect's `apply_chunk` is a no-op.

use routectl_core::{Message, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::lift_think_tags;

pub struct RawThinkTagDialect;
pub static RAW_THINK_TAG: RawThinkTagDialect = RawThinkTagDialect;

impl Dialect for RawThinkTagDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::RawThinkTag.format_tag()
    }

    fn apply_response(&self, id: &str, msg: &mut Message) -> Result<()> {
        lift_think_tags(id, msg, self.format_tag())
    }
}
