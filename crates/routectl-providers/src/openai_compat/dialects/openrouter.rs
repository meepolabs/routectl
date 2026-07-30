//! OpenRouter: responses already use the normalized `reasoning_details`
//! shape. Responses pass through unchanged; the outgoing request re-emits
//! the canonical `reasoning` object, which is OpenRouter's own native wire
//! shape (`reasoning: {effort|max_tokens|exclude|enabled}`).

use serde_json::Value;

use routectl_core::{ChatRequest, Error, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::preserve_history_reasoning_details;

/// OpenRouter reasoning dialect (see module docs).
pub struct OpenRouterDialect;
/// Shared instance of [`OpenRouterDialect`].
pub static OPENROUTER: OpenRouterDialect = OpenRouterDialect;

impl Dialect for OpenRouterDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::OpenRouter.format_tag()
    }

    /// Re-emit the canonical `reasoning` object on the wire. The request
    /// envelope strips `reasoning` unconditionally before dispatch because
    /// plain Chat Completions hosts 400 on the unknown key; OpenRouter's
    /// native request shape IS that object, so it must be put back here or
    /// the operator loses all reasoning control (effort / budget / exclude
    /// / enabled) with no signal. An all-empty config serializes to `{}`
    /// and is skipped rather than sent as a no-op key.
    fn apply_request(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
        req: &ChatRequest,
    ) -> Result<()> {
        if let Some(reasoning) = req.reasoning.as_ref() {
            let value = serde_json::to_value(reasoning)
                .map_err(|e| Error::normalize_request(id, e.to_string()))?;
            if value.as_object().is_some_and(|o| !o.is_empty()) {
                obj.insert("reasoning".into(), value);
            }
        }
        Ok(())
    }

    /// OpenRouter accepts the canonical / Anthropic-aligned typed
    /// `reasoning_details` array verbatim on echo-back. Preserve mode
    /// keeps the array intact and clears the legacy `reasoning` slot
    /// so the wire body has exactly one surface.
    fn preserve_history_reasoning(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
    ) -> Result<()> {
        preserve_history_reasoning_details(id, obj, self.format_tag())
    }
}
