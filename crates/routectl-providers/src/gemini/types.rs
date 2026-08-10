//! Google Gemini REST API wire types.
//!
//! Request side: Serialize-only structs for the `generateContent` body.
//! Response side: Deserialize-only structs for the `generateContent` response.
//!
//! Reference: <https://ai.google.dev/api/generate-content>

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Top-level request body for `POST models/{model}:generateContent`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentRequest {
    pub(crate) contents: Vec<Content>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) system_instruction: Option<SystemInstruction>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<GeminiTool>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_config: Option<ToolConfig>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) generation_config: Option<GenerationConfig>,
}

/// A conversation turn: one or more `parts` attributed to a `role`.
///
/// Gemini's `systemInstruction` has no `role` field; use `SystemInstruction`
/// for that. The `Content` type is only for `contents[]` entries.
#[derive(Debug, Serialize)]
pub struct Content {
    pub(crate) role: String,
    pub(crate) parts: Vec<Part>,
}

/// System prompt carrier. Gemini separates this from `contents[]` and
/// forbids a `role` field on it -- only `parts` is sent.
#[derive(Debug, Serialize)]
pub struct SystemInstruction {
    pub(crate) parts: Vec<Part>,
}

/// One part within a `Content`. Exactly one field is non-None per instance.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    /// Plain text content. When `thought` is true the text is the
    /// model's reasoning summary, replayed back on a follow-up turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,

    /// Inline binary data (images, audio, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inline_data: Option<InlineData>,

    /// Model-emitted tool call. Assistant turns carry these.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) function_call: Option<FunctionCallPart>,

    /// Tool result returned in a user turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) function_response: Option<FunctionResponsePart>,

    /// Marks this part as a thinking part rather than visible output.
    /// Only meaningful when replaying assistant reasoning back to the
    /// model alongside `thought_signature`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought: Option<bool>,

    /// Opaque signature paired with a `thought` part. Gemini emits it on
    /// reasoning parts and requires it verbatim on multi-turn replay.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thought_signature: Option<String>,
}

/// Binary payload embedded directly in the request.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineData {
    pub(crate) mime_type: String,
    pub(crate) data: String,
}

/// A function invocation emitted by the model in an assistant turn.
#[derive(Debug, Serialize)]
pub struct FunctionCallPart {
    pub(crate) name: String,
    pub(crate) args: Value,
}

/// A tool result returned by the caller in a user turn.
///
/// Gemini convention: tool results come back as a user-turn
/// `functionResponse` part (not a separate role). The `name` must match
/// the `functionCall.name` from the preceding assistant turn.
#[derive(Debug, Serialize)]
pub struct FunctionResponsePart {
    pub(crate) name: String,
    pub(crate) response: Value,
}

/// Tool definitions supplied to the model.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GeminiTool {
    pub(crate) function_declarations: Vec<FunctionDeclaration>,
}

/// One function the model may call.
#[derive(Debug, Serialize)]
pub struct FunctionDeclaration {
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parameters: Option<Value>,
}

/// Controls how the model invokes tools.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    pub(crate) function_calling_config: FunctionCallingConfig,
}

/// Function-calling mode and optional name filter.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCallingConfig {
    pub(crate) mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allowed_function_names: Option<Vec<String>>,
}

/// Sampling / output-shape parameters.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) temperature: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_p: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) top_k: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stop_sequences: Option<Vec<String>>,

    /// Sampler seed for reproducible output (canonical `seed`). Documented
    /// as a plain integer with no published width, so the canonical `i64`
    /// is forwarded as-is and an out-of-range value is upstream's to
    /// reject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seed: Option<i64>,

    /// Penalty applied to tokens already present in the response
    /// (canonical `presence_penalty`). The reference publishes no range for
    /// this endpoint, so the caller's value is forwarded unclamped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) presence_penalty: Option<f64>,

    /// Penalty scaled by how often a token has already appeared
    /// (canonical `frequency_penalty`). Forwarded unclamped for the same
    /// reason as `presence_penalty`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) frequency_penalty: Option<f64>,

    /// Output MIME type. Set to `application/json` to request structured
    /// output (canonical `response_format`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_mime_type: Option<String>,

    /// JSON schema constraining structured output. Paired with
    /// `response_mime_type = "application/json"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) response_schema: Option<Value>,

    /// Thinking controls (budget + whether thought summaries stream back).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_config: Option<ThinkingConfig>,
}

/// Thinking controls placed inside `generationConfig`.
///
/// `thinking_budget` and `thinking_level` are the two alternatives of the
/// wire `thinkingConfig` oneof: older Gemini generations take a numeric
/// budget, Gemini-3+ take a qualitative level string. Exactly one is ever
/// populated at build time, so `skip_serializing_if` guarantees the wire
/// carries at most one of them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    /// Token budget for the model's internal reasoning. `-1` requests a
    /// dynamic budget; `0` disables thinking on capable models. Used for
    /// pre-Gemini-3 generations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_budget: Option<i32>,

    /// Qualitative reasoning level for Gemini-3+ generations, the string
    /// alternative of the wire oneof. One of `minimal` | `low` | `medium`
    /// | `high`. Never serialized alongside `thinking_budget`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_level: Option<String>,

    /// When true, the model streams thought summaries (mapped to
    /// canonical reasoning) instead of hiding them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) include_thoughts: Option<bool>,
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

/// Top-level response from `generateContent`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerateContentResponse {
    #[serde(default)]
    pub(crate) candidates: Vec<Candidate>,

    #[serde(default)]
    pub(crate) usage_metadata: Option<UsageMetadata>,

    /// Returned model id (may differ from the requested id when the model
    /// was aliased or auto-upgraded by the API).
    #[serde(default)]
    pub(crate) model_version: Option<String>,

    /// Unique response id. Used as the canonical `ChatResponse.id`.
    #[serde(default)]
    pub(crate) response_id: Option<String>,

    /// Prompt-level feedback. When Gemini blocks the entire prompt before
    /// producing any candidate, it returns on the HTTP-200 surface with no
    /// candidates and a `blockReason` set here.
    #[serde(default)]
    pub(crate) prompt_feedback: Option<PromptFeedback>,
}

/// Prompt-level feedback carrying a policy `blockReason` when the prompt
/// itself was blocked (empty `candidates`). Deserialize-only; only
/// `block_reason` is consumed (safetyRatings / block message are not).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptFeedback {
    #[serde(default)]
    pub(crate) block_reason: Option<String>,
}

/// One candidate in the response. Non-streaming responses have one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub(crate) content: Option<ResponseContent>,
    pub(crate) finish_reason: Option<String>,
    /// Candidate index. Not read on either the streaming or the
    /// non-stream path -- both select a candidate positionally. Kept
    /// because the `u32` type-checks the value when the key IS present, so
    /// a non-integer `index` fails to parse; `serde(default)` means an
    /// absent key is accepted and reads as 0.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) index: u32,
}

/// Content block inside a candidate.
#[derive(Debug, Deserialize)]
pub struct ResponseContent {
    #[serde(default)]
    pub(crate) parts: Vec<ResponsePart>,
    /// Wire role ("model"). The canonical message is always emitted as
    /// `Role::Assistant`, so this is retained for wire fidelity only.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) role: Option<String>,
}

/// One part in a candidate's content. A part carries exactly one data field.
///
/// A part with `thought == true` carries reasoning text (and an opaque
/// `thought_signature` for multi-turn replay), not visible output.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponsePart {
    pub(crate) text: Option<String>,
    pub(crate) function_call: Option<ResponseFunctionCall>,
    /// True when `text` is a thinking summary rather than assistant output.
    #[serde(default)]
    pub(crate) thought: Option<bool>,
    /// Opaque signature for replaying this reasoning part on a later turn.
    #[serde(default)]
    pub(crate) thought_signature: Option<String>,
}

/// A function call emitted by the model in the response.
#[derive(Debug, Deserialize)]
pub struct ResponseFunctionCall {
    pub(crate) name: String,
    /// JSON object of arguments. Gemini returns this as a raw JSON object,
    /// not a serialized string -- we serialize to string for the canonical
    /// ToolCall.function.arguments field.
    #[serde(default)]
    pub(crate) args: Value,
}

/// Token count breakdown from the response.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub(crate) prompt_token_count: u32,

    #[serde(default)]
    pub(crate) candidates_token_count: u32,

    #[serde(default)]
    pub(crate) total_token_count: u32,

    /// Tokens served from the context cache (implicit or explicit).
    /// Maps to `cache_read_input_tokens` in the canonical Usage.
    #[serde(default)]
    pub(crate) cached_content_token_count: u32,

    /// Reasoning tokens emitted by thinking-enabled models.
    /// Maps to `reasoning_tokens` in the canonical Usage.
    /// Slice-2 will surface these on the thinking path.
    #[serde(default)]
    pub(crate) thoughts_token_count: u32,

    /// Tokens consumed by tool-use prompts (Gemini-internal). Parsed for
    /// wire fidelity; not separately modeled in the canonical `Usage`.
    #[serde(default)]
    #[allow(dead_code)]
    pub(crate) tool_use_prompt_token_count: u32,
}
