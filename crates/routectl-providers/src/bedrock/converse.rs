//! Converse adapter -- AWS-normalized request/response shape that
//! works across Bedrock vendors (Anthropic, Mistral, Llama, Cohere, ...).
//!
//! The Converse envelope:
//!
//! ```json
//! {
//!   "messages": [{"role": "user", "content": [{"text": "..."}]}],
//!   "system": [{"text": "..."}],
//!   "inferenceConfig": {"maxTokens": 4096, "temperature": 0.7},
//!   "toolConfig": {...},
//!   "additionalModelRequestFields": {"anthropic_beta": [...]}
//! }
//! ```
//!
//! AWS handles per-vendor body translation internally, so adding a new
//! Bedrock-hosted vendor doesn't require a new adapter on routectl's
//! side -- just point a provider at the new model id with `api_shape =
//! "converse"` and AWS does the rest.

use routectl_core::{ChatRequest, ChatResponse, Error, Result};
use serde_json::Value;

use super::BedrockConfig;

/// Build the Converse request body from a routectl `ChatRequest`.
pub fn normalize_request(_cfg: &BedrockConfig, _req: &ChatRequest) -> Result<Value> {
    Err(Error::Config(
        "bedrock converse::normalize_request not implemented yet (M2.7)".into(),
    ))
}

/// Parse the Bedrock Converse response body into a `ChatResponse`.
pub fn normalize_response(_provider_id: &str, _raw: Value) -> Result<ChatResponse> {
    Err(Error::Config(
        "bedrock converse::normalize_response not implemented yet (M2.7)".into(),
    ))
}
