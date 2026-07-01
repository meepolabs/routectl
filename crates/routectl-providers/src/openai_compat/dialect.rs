//! Reasoning dialect enum and per-dialect format tag strings.

/// Identifies which reasoning wire-format quirks a given endpoint uses.
/// Chosen once per `OpenAiCompatConfig` at construction time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningDialect {
    /// Vanilla OpenAI o-series: `reasoning_effort` request param; reasoning
    /// is hidden inside the completion, not surfaced in the response body
    /// (unless `reasoning_content` sneaks through, in which case we lift it).
    OpenAi,

    /// DeepSeek: `reasoning_content` field on the response message.
    /// Must be stripped from outgoing message history or the API returns 400.
    DeepSeek,

    /// vLLM-served models (Qwen3, MiMo, etc.): `chat_template_kwargs` for
    /// enabling thinking; `reasoning_content` field on response same as DeepSeek.
    Vllm,

    /// Endpoints that emit `<think>...</think>` inline in the content string
    /// (llama.cpp default for QwQ/DeepSeek when served without special handling).
    RawThinkTag,

    /// OpenRouter upstream: responses already use the normalized
    /// `reasoning_details` shape; pass through unmodified.
    OpenRouter,

    /// Generic passthrough -- no reasoning mutations in either direction.
    Passthrough,
}

impl ReasoningDialect {
    /// The `format` tag written into every `ReasoningDetail` produced by
    /// this dialect. Consumers can use this to re-route to the right
    /// continuation logic.
    pub const fn format_tag(self) -> &'static str {
        match self {
            Self::OpenAi => "openai-responses-v1",
            Self::DeepSeek => "deepseek-v1",
            Self::Vllm => "vllm-reasoning-v1",
            Self::RawThinkTag => "raw-think-tag-v1",
            Self::OpenRouter => "openrouter-passthrough-v1",
            Self::Passthrough => "passthrough-v1",
        }
    }

    /// Returns true if outgoing message history must have `reasoning_content`
    /// and `reasoning_details` stripped before sending to the upstream.
    pub const fn strip_history_reasoning(self) -> bool {
        matches!(self, Self::DeepSeek | Self::Vllm)
    }

    /// Returns true if the response carries a `reasoning_content` field that
    /// must be lifted into `reasoning_details`.
    pub const fn lifts_reasoning_content(self) -> bool {
        matches!(self, Self::DeepSeek | Self::Vllm)
    }
}

/// Default required by `OpenAiCompatConfig`'s `Default::default()`
/// fallback for library consumers that don't pin a dialect.
impl Default for ReasoningDialect {
    fn default() -> Self {
        Self::OpenAi
    }
}

/// Map the cross-crate carrier enum (`routectl_core`) into the
/// providers-private dispatch enum. Keeps the carrier on
/// `ChatRequest::routectl_internal` crate-neutral while letting this
/// crate's dispatch loop use a tighter (#[non_exhaustive]) shape.
impl From<routectl_core::CoreReasoningDialect> for ReasoningDialect {
    fn from(d: routectl_core::CoreReasoningDialect) -> Self {
        match d {
            routectl_core::CoreReasoningDialect::Openai => Self::OpenAi,
            routectl_core::CoreReasoningDialect::Deepseek => Self::DeepSeek,
            routectl_core::CoreReasoningDialect::Vllm => Self::Vllm,
            routectl_core::CoreReasoningDialect::RawThinkTag => Self::RawThinkTag,
            routectl_core::CoreReasoningDialect::Openrouter => Self::OpenRouter,
            routectl_core::CoreReasoningDialect::Passthrough => Self::Passthrough,
        }
    }
}
