//! OpenRouter-normalized request/response schema.
//!
//! Shape reference: <https://openrouter.ai/docs/guides/best-practices/reasoning-tokens>
//!
//! Key design choice (DEC-001): routectl's outward schema mirrors OpenRouter so
//! any client that speaks OpenRouter speaks routectl. Reasoning is first-class:
//! `reasoning` config in request, `reasoning_details` array in response.
//!
//! v0.4.0 extension (DEC-002): the canonical now carries Anthropic-shape
//! features (cache_control on every block, top-level system, anthropic_beta,
//! cache usage stats) so an Anthropic-in / Anthropic-out and Anthropic-in /
//! Bedrock-Invoke-out request round-trips losslessly. Typed `ContentPart`,
//! `SystemContent`, and `ToolDef` replace the earlier `Vec<Value>`
//! passthroughs. See `crate::content_part`, `crate::system_content`,
//! `crate::tool_def`, `crate::cache_control`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache_control::CacheControl;
use crate::content_part::ContentPart;
use crate::system_content::SystemContent;
use crate::tool_def::ToolDef;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,

    /// Top-level system prompt. Anthropic accepts a flat string or an
    /// array of typed text blocks with per-block `cache_control`. The
    /// OpenAI ingress lifts `Role::System` messages into this field at
    /// parse time; egresses read it directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemContent>,

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

    /// Tool definitions. Typed `ToolDef::Custom` for canonical custom
    /// tools (with `cache_control`, `defer_loading`, `strict`); typed
    /// `ToolDef::Other(Value)` for OpenAI-shape function tools,
    /// Anthropic builtins, and any future shape (passthrough).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<Value>,

    /// Top-level cache breakpoint (auto-cache mode). Counts toward the
    /// 4-breakpoint cap. Anthropic-only; egresses without prompt caching
    /// drop with a `tracing::warn!`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,

    /// Body-level Anthropic beta flags (e.g. `context-1m-2025-08-07`).
    /// Egresses to Anthropic-shape upstreams (Anthropic API,
    /// Bedrock-Invoke) merge this into the body's `anthropic_beta` array.
    /// Distinct from the `anthropic-beta` HTTP header which is configured
    /// per provider via `extra_headers`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anthropic_beta: Vec<String>,

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
    /// Typed content parts. Round-trips Anthropic and OpenAI-shape blocks
    /// losslessly via `ContentPart` (see `crate::content_part`). Unknown
    /// block types fall to `ContentPart::Other` which preserves the
    /// original `type` discriminant and arbitrary fields.
    Parts(Vec<ContentPart>),
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

/// Usage tallies. v0.4.0 extension: cache stats from Anthropic /
/// Bedrock-Invoke responses surface here (`cache_creation_input_tokens`,
/// `cache_read_input_tokens`, and the per-TTL breakdown).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    /// Tokens written to the prompt cache on this request (cache miss
    /// path). Anthropic / Bedrock-Invoke only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    /// Tokens read from the prompt cache on this request (cache hit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    /// Per-TTL breakdown of cache creations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
}

/// Per-TTL breakdown of cache writes for one request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheCreation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_5m_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ephemeral_1h_input_tokens: Option<u32>,
}

/// Streaming SSE chunk (delta).
///
/// `id` and `model` are tolerated as missing on the wire: some upstreams
/// emit cost/usage trailer chunks where these fields are absent. Empty
/// strings serialize back out, which is fine for OpenAI-style SSE clients
/// that only look at `choices[].delta`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatChunk {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Streaming usage update. Anthropic emits cache stats in
    /// `message_delta` events; routectl surfaces them here so OpenAI-SSE
    /// clients see the same totals at end-of-stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageDelta>,
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

/// Streaming usage delta. Mirrors `Usage` but every field is optional
/// because chunks may carry partial info.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageDelta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation: Option<CacheCreation>,
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

/// Discriminator on a `ReasoningDetail`. Determines what fields the
/// detail's `payload` object carries and how downstream egresses
/// interpret it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningDetailKind {
    /// OpenAI Responses reasoning summary block. `payload.text`
    /// carries a one-paragraph summary the model surfaces alongside
    /// the answer; not the full chain-of-thought.
    #[serde(rename = "reasoning.summary")]
    Summary,
    /// OpenAI Responses encrypted reasoning. `payload.encrypted_content`
    /// is an opaque blob the model emits and expects back verbatim on
    /// follow-up turns for chain-of-thought continuity. Round-trip
    /// only; never displayed to the user.
    #[serde(rename = "reasoning.encrypted")]
    Encrypted,
    /// Anthropic-shape thinking block. `payload.text` is the visible
    /// thinking content; `payload.signature` is mandatory for
    /// multi-turn replay (Anthropic 400s on follow-ups missing it).
    /// Format string `anthropic-claude-v1` distinguishes from other
    /// `Text`-kind details.
    #[serde(rename = "reasoning.text")]
    Text,
}
