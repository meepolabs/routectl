//! OpenRouter-normalized request/response schema.
//!
//! Shape reference: <https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>
//!
//! Key design choice (DEC-001): routectl's outward schema mirrors OpenRouter so
//! any client that speaks OpenRouter speaks routectl. Reasoning is first-class:
//! `reasoning` config in request, `reasoning_details` array in response.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub n: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logprobs: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_logprobs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logit_bias: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,

    /// Unified reasoning controls. Translated per-provider in `Provider::normalize_request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningConfig>,

    /// Server-side chat-template kwargs (vLLM, DashScope, some NIM endpoints).
    /// Forwarded as-is to providers that accept them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_template_kwargs: Option<Value>,

    /// Long-tail provider knobs we don't normalize. Merged into upstream body verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_extras: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,

    /// Echoed reasoning from a prior assistant turn (legacy plaintext shape).
    /// Providers may strip before resending (DeepSeek 400s on this).
    /// Upstream `reasoning_content` (DeepSeek/vLLM/NIM shape) is coalesced
    /// into this field by the openai-compat normalizer's preprocess step.
    /// We don't use a serde alias here because NIM sometimes emits BOTH
    /// keys (one of them null), which would deserialize-fail with
    /// "duplicate field `reasoning`".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,

    /// Echoed reasoning from a prior assistant turn (typed-blocks shape).
    /// Anthropic tool-use loops require these to be passed back unmodified.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<ReasoningDetail>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    /// OpenAI-style content parts: [{type: "text", text: "..."}, {type: "image_url", ...}]
    Parts(Vec<Value>),
    /// Some upstreams (Clarifai-hosted models on OpenRouter, vLLM trailers)
    /// return `"content": null` when the entire output is reasoning. We
    /// accept it on the wire and serialize back as null.
    Null,
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text(String::new())
    }
}

/// Unified reasoning request config. See OpenRouter docs for provider mapping.
///
/// - OpenAI o-series: `effort` -> `reasoning_effort`
/// - Anthropic: `max_tokens` -> `thinking.budget_tokens` (or `effort` mapped)
/// - DeepSeek: model selection (`-reasoner` variant)
/// - Qwen / vLLM: `enabled` -> `chat_template_kwargs.enable_thinking`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// "minimal" | "low" | "medium" | "high" | "xhigh" | "none"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Anthropic/Gemini-style budget. Mutually exclusive with `effort` per OpenRouter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Suppress reasoning content from response (still billed for tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<bool>,
    /// Enable reasoning with provider defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// `id`, `model`, and `created` are tolerated as missing on the wire:
/// some upstreams (e.g. NIM's gemma-3) omit `created` entirely, and
/// `id`/`model` may be absent on minimal responses. Empty strings and a
/// zero timestamp serialize back out, which is acceptable for OpenAI-style
/// clients that treat these fields as informational.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub created: i64,
    pub choices: Vec<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Which configured provider answered (routectl-specific extension; clients ignore).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routectl_provider: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
}

/// Streaming SSE chunk (delta).
///
/// `id` and `model` are tolerated as missing on the wire: some upstreams
/// emit cost/usage trailer chunks where these fields are absent. Empty
/// strings serialize back out, which is fine for OpenAI-style SSE clients
/// that only look at `choices[].delta`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkChoice {
    pub index: u32,
    pub delta: ChunkDelta,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChunkDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<Role>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// Upstream `reasoning_content` is coalesced here by the SSE chunk
    /// preprocessor; see `coalesce_reasoning_content` in openai_compat/sse.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_details: Vec<ReasoningDetail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
}

/// Top-level reasoning content on an assistant message. Mirrors OpenRouter's
/// dual-shape: legacy `reasoning` string + typed `reasoning_details` array.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reasoning {
    /// Legacy plaintext form. Suitable for single-turn / simple workflows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Typed blocks. Required for Anthropic tool-use continuity and
    /// any encrypted/redacted reasoning payload.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<ReasoningDetail>,
}

/// One block of reasoning. `format` carries the provider-specific shape tag
/// (e.g. `"anthropic-claude-v1"`, `"openai-responses-v1"`, `"deepseek-v1"`).
///
/// `id`, `format`, and `index` are optional on the wire: OpenRouter omits
/// `id` for plain text reasoning blocks, and some upstreams never set
/// `format`/`index`. We accept the looser shape and let normalizers fill
/// defaults when they need them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningDetail {
    #[serde(rename = "type")]
    pub kind: ReasoningDetailKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(flatten)]
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDetailKind {
    #[serde(rename = "reasoning.summary")]
    Summary,
    #[serde(rename = "reasoning.encrypted")]
    Encrypted,
    #[serde(rename = "reasoning.text")]
    Text,
}
