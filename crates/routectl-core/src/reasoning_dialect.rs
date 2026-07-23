//! Per-model openai-compat reasoning knobs that travel from the router
//! through the dispatch layer into the egress.
//!
//! These enums live in `routectl-core` (rather than `routectl-providers`)
//! because the `RoutectlInternal` carrier on `ChatRequest` references
//! them, and `routectl-core` cannot depend on `routectl-providers`
//! (the dep direction is the other way). The providers crate's
//! private dispatch enum (`openai_compat::ReasoningDialect`,
//! `openai_compat::HistoryReasoning`) maps to/from these one-to-one
//! through `From` impls; that boundary keeps the providers crate's
//! internal dispatch types free to evolve without touching the
//! cross-crate carrier surface.

/// Per-model reasoning dialect. Mirrors the values on the
/// providers-side `openai_compat::ReasoningDialect` enum and on the
/// router-side `config::ReasoningDialect` enum; each crate maps to
/// this canonical form through its own `From` impl so the carrier on
/// `ChatRequest::routectl_internal` stays crate-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReasoningDialect {
    /// Vanilla OpenAI o-series: reasoning is hidden inside the completion
    /// and not surfaced in the response body.
    #[default]
    Openai,
    /// DeepSeek: `reasoning_content` field on the response message,
    /// stripped from outgoing history.
    Deepseek,
    /// vLLM-served models: thinking enabled via `chat_template_kwargs`,
    /// `reasoning_content` on the response as with DeepSeek.
    Vllm,
    /// Endpoints that emit `<think>...</think>` inline in the content
    /// string.
    RawThinkTag,
    /// OpenRouter upstream: responses already use the normalized
    /// `reasoning_details` shape; passed through unmodified.
    Openrouter,
    /// Generic passthrough: no reasoning mutations in either direction.
    Passthrough,
}

/// Per-model outgoing-history reasoning policy for openai-compat
/// providers. Mirrors the providers-side
/// `openai_compat::HistoryReasoning` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryReasoning {
    /// Use the dialect's default strip-or-preserve behavior.
    #[default]
    Auto,
    /// Force-strip reasoning fields from outgoing assistant messages.
    Strip,
    /// Force-emit the dialect-native preserve shape on outgoing assistant
    /// messages.
    Preserve,
}
