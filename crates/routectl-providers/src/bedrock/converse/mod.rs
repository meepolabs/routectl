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

pub(crate) mod eventstream;
mod extras;
mod messages;
mod request;
mod response;
mod response_types;
mod system;
mod tools;
mod types;

pub(super) use eventstream::stream as eventstream_stream;

use serde_json::Value;

use routectl_core::{ChatRequest, ChatResponse, Error, Result};

use super::BedrockConfig;

/// Provider-kind discriminator string for this Bedrock shape. Defined
/// HERE, on the surface that owns the shape, and read back by
/// `BedrockApiShape::provider_kind_str` -- the Bedrock egress carries two
/// shapes, so the shape's own module is the one place that can hold the
/// spelling without a second module needing to agree with it.
pub(crate) const PROVIDER_KIND: &str = "bedrock-converse";

/// The lane spelling every `record_translation_drop` /
/// `record_translation_policy_action` / `record_translation_lane_seen`
/// call on this egress uses. Bound to `PROVIDER_KIND` rather than
/// re-spelled so the telemetry lane and the `provider_kind=` tracing
/// field can never drift apart.
pub(crate) const LANE: &str = PROVIDER_KIND;

/// Build the Converse request body from a routectl `ChatRequest`.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    let cr = request::translate(cfg, req)?;
    serde_json::to_value(&cr).map_err(|e| Error::normalize_request(&cfg.id, e.to_string()))
}

/// Parse the Bedrock Converse response body into a `ChatResponse`.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    response::translate(provider_id, &raw)
}
