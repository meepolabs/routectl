//! Generic passthrough -- no reasoning mutations in either direction.
//! Useful for endpoints we have no specific knowledge of.

use super::super::dialect::ReasoningDialect;
use super::Dialect;

pub struct PassthroughDialect;
pub static PASSTHROUGH: PassthroughDialect = PassthroughDialect;

impl Dialect for PassthroughDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::Passthrough.format_tag()
    }
}
