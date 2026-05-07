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
    pub fn format_tag(self) -> &'static str {
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
    pub fn strip_history_reasoning(self) -> bool {
        matches!(self, Self::DeepSeek | Self::Vllm)
    }

    /// Returns true if the response carries a `reasoning_content` field that
    /// must be lifted into `reasoning_details`.
    pub fn lifts_reasoning_content(self) -> bool {
        matches!(self, Self::DeepSeek | Self::Vllm)
    }
}
