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
    #[default]
    Openai,
    Deepseek,
    Vllm,
    RawThinkTag,
    Openrouter,
    Passthrough,
}

/// Per-model outgoing-history reasoning policy for openai-compat
/// providers. Mirrors the providers-side
/// `openai_compat::HistoryReasoning` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryReasoning {
    #[default]
    Auto,
    Strip,
    Preserve,
}
