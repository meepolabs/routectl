//! OpenRouter: responses already use the normalized `reasoning_details`
//! shape. No request mutations, no response lifting -- requests and
//! responses pass through unchanged.

use serde_json::Value;

use routectl_core::Result;

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
