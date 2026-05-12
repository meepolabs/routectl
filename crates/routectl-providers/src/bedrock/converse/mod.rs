//! AWS Bedrock Converse adapter -- vendor-neutral envelope.
//!
//! Converse is AWS's vendor-agnostic body shape: callers send the same
//! `{messages, system, inferenceConfig, toolConfig,
//! additionalModelRequestFields}` envelope regardless of the underlying
//! model (Anthropic, Mistral, Cohere, Meta, ...) and AWS handles the
//! per-vendor body translation server-side. The big payoff: adding a
//! new Bedrock-hosted vendor becomes a config change, not a code change.
//!
//! Wire differences from the Anthropic-shape Invoke body
//! (`crates/routectl-providers/src/bedrock/invoke.rs`):
//!
//! - Top-level: `{messages, system, inferenceConfig, toolConfig,
//!   additionalModelRequestFields, additionalModelResponseFieldPaths}`.
//! - `inferenceConfig` is camelCase: `{maxTokens, temperature, topP,
//!   stopSequences}`.
//! - `system` is an array of single-key blocks: `[{text}|{cachePoint}]`,
//!   not a flat string.
//! - Content blocks are AWS-shape single-key unions: `{text}` |
//!   `{image:{format,source:{bytes:base64}}}` |
//!   `{toolUse:{toolUseId,name,input}}` |
//!   `{toolResult:{toolUseId,content:[...],status?}}` |
//!   `{cachePoint:{type:"default"}}`.
//! - `toolConfig.tools[]` are `{toolSpec:{name,description?,
//!   inputSchema:{json:...}}}` (or `{cachePoint}` for cache breakpoints
//!   between tools).
//! - `toolChoice` is the union `{auto:{}}` | `{any:{}}` |
//!   `{tool:{name}}`.
//! - cache_control + anthropic_beta + thinking config land in
//!   `additionalModelRequestFields`, which AWS forwards verbatim to
//!   Anthropic models.
//!
//! Helper reuse: this module borrows the canonical -> Anthropic-shape
//! translation primitives from `crate::anthropic_api::request`
//! (`translate_system`, `translate_tool`, `translate_custom_tool`,
//! `build_thinking`) and adapts the result to the AWS Converse shape.
//! Forward-compat catchalls in `ContentPart::Other` and `ToolDef::Other`
//! are handled with the same warn-or-error policy as the openai-compat
//! egress.
//!
//! Scope: M5.A built request + types; M5.B (this module's `response.rs`
//! and `eventstream.rs`) covers the response side. M5.C (dispatch
//! wiring + drop-three-startup-guards + live matrix row) follows.

mod eventstream;
mod extras;
mod messages;
mod request;
mod response;
mod response_types;
mod system;
mod tools;
mod types;

pub use types::*;

// Public re-exports of response-side types so other modules in the
// bedrock crate (and integration tests) can name them without the
// inner `response_types::` path.
pub use response_types::{
    ConverseCacheDetail, ConverseMetrics, ConverseOutput, ConverseReasoningContent,
    ConverseReasoningText, ConverseResponse, ConverseResponseContentBlock, ConverseResponseMessage,
    ConverseResponseToolUse, ConverseUsage, StreamContentBlockDelta, StreamContentBlockStart,
    StreamContentBlockStartPayload, StreamContentBlockStop, StreamDelta, StreamMessageStart,
    StreamMessageStop, StreamMetadata, StreamReasoningDelta, StreamToolUseDelta,
    StreamToolUseStart,
};

// Re-export the eventstream entry point so `super::eventstream::
// converse_stream` can delegate without exposing the inner module
// publicly to the rest of the providers crate.
pub(super) use eventstream::stream as eventstream_stream;

use serde_json::Value;

use routectl_core::{ChatRequest, ChatResponse, Error, Result};

use super::BedrockConfig;

/// Build the Converse request body from a routectl `ChatRequest`.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    let cr = request::translate(cfg, req)?;
    serde_json::to_value(&cr).map_err(|e| Error::normalize_request(&cfg.id, e.to_string()))
}

/// Parse the Bedrock Converse response body into a `ChatResponse`.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    response::translate(provider_id, &raw)
}
