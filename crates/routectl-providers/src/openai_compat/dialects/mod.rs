//! Per-dialect behavior, organized as one struct per dialect implementing
//! the [`Dialect`] trait. The dispatch table at the bottom of this module
//! maps the [`ReasoningDialect`] enum to a static reference of the
//! corresponding impl, so callers in `request.rs`/`response.rs`/`sse.rs`
//! can do `dialect.as_dyn().apply_request(...)` instead of matching on
//! the enum themselves.
//!
//! Adding a new dialect:
//!   1. Drop a new file in this directory with one struct + impl Dialect.
//!   2. Add a variant to [`ReasoningDialect`] in `../dialect.rs`.
//!   3. Add an arm to [`ReasoningDialect::as_dyn`] below.

pub mod deepseek;
pub mod openai;
pub mod openrouter;
pub mod passthrough;
pub mod raw_think_tag;
pub(crate) mod util;
pub mod vllm;

use serde_json::Value;

use routectl_core::{ChatRequest, Message, Result};

use super::dialect::ReasoningDialect;

/// Per-dialect normalization hooks. All methods take `&self` and mutate
/// their inputs by reference, so impls can be zero-sized statics.
///
/// Default impls are no-ops -- a Passthrough-style dialect can implement
/// only `format_tag` and inherit the rest.
pub trait Dialect: Send + Sync {
    /// The `format` tag written into every `ReasoningDetail` produced by
    /// this dialect. Stable across versions; clients may use it to route
    /// to the right continuation logic.
    fn format_tag(&self) -> &'static str;

    /// Returns true if outgoing message history must have
    /// `reasoning_content` / `reasoning_details` / `reasoning` stripped
    /// before sending to the upstream (DeepSeek and vLLM 4xx otherwise).
    fn strip_history_reasoning(&self) -> bool {
        false
    }

    /// Mutate outgoing message history to preserve reasoning in the
    /// dialect-native shape:
    ///   - DeepSeek / Vllm: rename canonical `reasoning` to wire
    ///     `reasoning_content` (DeepSeek v4+ requires echo-back).
    ///   - OpenRouter: keep `reasoning_details` typed array as-is.
    ///   - OpenAI / Passthrough / RawThinkTag: default no-op (no
    ///     well-defined preserve shape on the wire).
    ///
    /// Called by the egress runtime when `history_reasoning =
    /// "preserve"` is explicitly set on the provider, OR when the
    /// dialect's default (`history_reasoning = "auto"`) is preserve.
    /// Today `auto` defaults to strip for DeepSeek/Vllm and to no-op
    /// for the others, so this method only fires on explicit operator
    /// opt-in.
    fn preserve_history_reasoning(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
    ) -> Result<()> {
        let _ = (id, obj);
        Ok(())
    }

    /// Returns true if the response carries a `reasoning_content` field
    /// that should be lifted into `reasoning_details`. Used as a sanity
    /// flag for tests; the actual lifting happens in `apply_response`.
    fn lifts_reasoning_content(&self) -> bool {
        false
    }

    /// Mutate the outgoing request body in place: inject reasoning
    /// params, strip unsupported fields, etc. The serialized
    /// `ChatRequest` body is provided as a JSON object so dialect impls
    /// can edit any key.
    fn apply_request(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
        req: &ChatRequest,
    ) -> Result<()> {
        let _ = (id, obj, req);
        Ok(())
    }

    /// Mutate the deserialized response message in place: lift
    /// reasoning fields into `reasoning_details`, strip embedded
    /// `<think>` tags from content, etc.
    fn apply_response(&self, id: &str, msg: &mut Message) -> Result<()> {
        let _ = (id, msg);
        Ok(())
    }

    /// Mutate one parsed SSE chunk JSON in place. Called once per
    /// chunk after generic shape coalescing but before final
    /// deserialization into `ChatChunk`.
    fn apply_chunk(&self, id: &str, val: &mut Value) -> Result<()> {
        let _ = (id, val);
        Ok(())
    }
}

impl ReasoningDialect {
    /// Resolve to the static `Dialect` impl for this variant.
    /// All impls are zero-sized statics, so this is essentially a
    /// `match` returning `&'static dyn Dialect` -- no allocation.
    pub fn as_dyn(self) -> &'static dyn Dialect {
        match self {
            Self::OpenAi => &openai::OPENAI,
            Self::DeepSeek => &deepseek::DEEPSEEK,
            Self::Vllm => &vllm::VLLM,
            Self::RawThinkTag => &raw_think_tag::RAW_THINK_TAG,
            Self::OpenRouter => &openrouter::OPENROUTER,
            Self::Passthrough => &passthrough::PASSTHROUGH,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_resolves_to_a_dyn() {
        // Smoke test: each enum variant's as_dyn() must return a
        // non-default format_tag. If a new variant is added without a
        // dialects/* impl, this test fails to compile or returns "".
        for variant in [
            ReasoningDialect::OpenAi,
            ReasoningDialect::DeepSeek,
            ReasoningDialect::Vllm,
            ReasoningDialect::RawThinkTag,
            ReasoningDialect::OpenRouter,
            ReasoningDialect::Passthrough,
        ] {
            let tag = variant.as_dyn().format_tag();
            assert!(!tag.is_empty(), "{variant:?} has empty format_tag");
            assert_eq!(tag, variant.format_tag(), "trait/enum tags must agree");
        }
    }
}
