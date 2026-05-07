//! OpenRouter: responses already use the normalized `reasoning_details`
//! shape. No request mutations, no response lifting -- requests and
//! responses pass through unchanged.

use super::super::dialect::ReasoningDialect;
use super::Dialect;

pub struct OpenRouterDialect;
pub static OPENROUTER: OpenRouterDialect = OpenRouterDialect;

impl Dialect for OpenRouterDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::OpenRouter.format_tag()
    }
}
