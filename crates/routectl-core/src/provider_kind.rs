//! Provider-kind vocabularies.
//!
//! Two spellings exist and must not be mixed:
//!
//! - [`config`]: the `kind = "..."` token of a `[providers.X]` block. The
//!   usage ledger persists it per row, so its values are a storage format.
//! - [`trace`]: the `provider_kind=` field a provider writes on its own
//!   tracing lines (`anthropic`, never `anthropic-api`; each Bedrock shape
//!   apart). Fixture captures and log evidence key on these values.

use crate::wire_check::EgressKind;

/// Config-key provider kinds, as a `[providers.X]` block spells them.
pub mod config {
    /// The direct Anthropic Messages API.
    pub const ANTHROPIC_API: &str = "anthropic-api";
    /// An OpenAI-compatible Chat Completions endpoint.
    pub const OPENAI_COMPAT: &str = "openai-compat";
    /// Native AWS Bedrock (either API shape).
    pub const BEDROCK: &str = "bedrock";
    /// The OpenAI Responses API.
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    /// Gemini.
    pub const GEMINI: &str = "gemini";
}

/// Provider-kind tokens providers write in their `provider_kind=` trace
/// fields.
pub mod trace {
    /// The Anthropic Messages provider.
    pub const ANTHROPIC: &str = "anthropic";
    /// Bedrock InvokeModel.
    pub const BEDROCK_INVOKE: &str = "bedrock-invoke";
    /// Bedrock Converse.
    pub const BEDROCK_CONVERSE: &str = "bedrock-converse";
    /// The OpenAI-compatible Chat Completions provider.
    pub const OPENAI_COMPAT: &str = "openai-compat";
    /// The OpenAI Responses provider.
    pub const OPENAI_RESPONSES: &str = "openai-responses";
    /// The Gemini provider.
    pub const GEMINI: &str = "gemini";
}

/// The [`trace`] token the provider serving `egress` writes on its request
/// lines. A Mantle sub-lane is served by its standard sibling's provider;
/// `Local` sends nothing upstream and has no token.
#[must_use]
pub const fn trace_kind(egress: EgressKind) -> Option<&'static str> {
    match egress {
        EgressKind::AnthropicApi | EgressKind::AnthropicApiMantle => Some(trace::ANTHROPIC),
        EgressKind::BedrockInvoke => Some(trace::BEDROCK_INVOKE),
        EgressKind::BedrockConverse => Some(trace::BEDROCK_CONVERSE),
        EgressKind::OpenaiCompat | EgressKind::OpenaiCompatMantle => Some(trace::OPENAI_COMPAT),
        EgressKind::OpenaiResponses | EgressKind::OpenaiResponsesMantle => {
            Some(trace::OPENAI_RESPONSES)
        }
        EgressKind::Gemini => Some(trace::GEMINI),
        EgressKind::Local => None,
    }
}

#[cfg(test)]
#[path = "provider_kind_tests.rs"]
mod tests;
