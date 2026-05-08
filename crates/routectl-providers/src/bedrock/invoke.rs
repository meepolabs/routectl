//! InvokeModel adapter -- vendor-specific request body, with Anthropic
//! Messages JSON for Claude models.
//!
//! For Anthropic on Bedrock, the body shape is identical to the
//! Anthropic Messages API except:
//!   - `anthropic_version` is `"bedrock-2023-05-31"` (Bedrock-specific
//!     version string) and lives in the BODY, not in a header.
//!   - No auth headers (replaced by SigV4 signing on the HTTP layer).
//!   - Beta flags travel in the body's `anthropic_beta` array.
//!
//! We delegate body construction to `anthropic_api::request::normalize`
//! so the reasoning / cache_control / tool-use logic stays in one place,
//! then patch the result for Bedrock-specific fields.
//!
//! Response parsing is a direct call to `anthropic_api::response::normalize`
//! -- the Bedrock InvokeModel response shape for Anthropic models is
//! identical to the Anthropic Messages API response shape (which is
//! the whole point of the Invoke shape: pure passthrough).

use serde_json::Value;

use routectl_core::{ChatRequest, ChatResponse, Error, Result};

use super::BedrockConfig;

/// The Bedrock-required `anthropic_version` body field. Distinct from
/// the Anthropic API's `anthropic-version: 2023-06-01` header.
const BEDROCK_ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

/// Build the InvokeModel request body from a routectl `ChatRequest`.
///
/// For Anthropic Claude models the body is Anthropic Messages JSON
/// patched with Bedrock-specific fields. For non-Anthropic Bedrock-hosted
/// vendors, prefer Converse (vendor-neutral) -- the InvokeModel adapter
/// here does not currently shape Mistral/Llama/Cohere bodies.
pub fn normalize_request(cfg: &BedrockConfig, req: &ChatRequest) -> Result<Value> {
    let mut body = crate::anthropic_api::request::normalize(&cfg.id, req)?;
    let obj = body.as_object_mut().ok_or_else(|| {
        Error::NormalizeRequest(
            cfg.id.clone(),
            "anthropic_api::request::normalize returned non-object".into(),
        )
    })?;

    // Bedrock requires the body-side anthropic_version. Override
    // anything the upstream normalizer may have added.
    obj.insert(
        "anthropic_version".into(),
        Value::String(BEDROCK_ANTHROPIC_VERSION.into()),
    );

    // Merge the configured beta flags. If the body already has its own
    // anthropic_beta (e.g. from a per-request override), prepend the
    // provider-level flags so user overrides take precedence on
    // duplicate keys.
    if !cfg.anthropic_beta.is_empty() {
        let combined: Vec<Value> = cfg
            .anthropic_beta
            .iter()
            .cloned()
            .map(Value::String)
            .chain(
                obj.get("anthropic_beta")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            )
            .collect();
        obj.insert("anthropic_beta".into(), Value::Array(combined));
    }

    // Merge any additional model request fields at the top level. The
    // user is responsible for not stomping on routectl-managed keys
    // (`messages`, `system`, etc.).
    if let Some(extras) = cfg.additional_model_request_fields.as_ref() {
        if let Some(extras_obj) = extras.as_object() {
            for (k, v) in extras_obj {
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    // Stream flag is decided at HTTP level (different endpoint suffix),
    // not via a body field, so strip any leftover.
    obj.remove("stream");

    Ok(body)
}

/// Parse the Bedrock InvokeModel response body into a `ChatResponse`.
///
/// For Anthropic Claude models this is exactly the Anthropic Messages
/// API response shape, so we delegate.
pub fn normalize_response(provider_id: &str, raw: Value) -> Result<ChatResponse> {
    crate::anthropic_api::response::normalize(provider_id, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    use crate::bedrock::{BedrockApiShape, BedrockCreds};

    fn fake_cfg() -> BedrockConfig {
        BedrockConfig {
            id: "bedrock:test".into(),
            region: "us-west-2".into(),
            model_id: "anthropic.claude-haiku-4-5".into(),
            api_shape: BedrockApiShape::Invoke,
            creds: BedrockCreds::BearerKey { key: "test".into() },
            user_agent: None,
            extra_headers: Vec::new(),
            anthropic_beta: vec!["context-1m-2025-08-07".into()],
            additional_model_request_fields: Some(json!({"top_p": 0.9})),
        }
    }

    fn user_req() -> ChatRequest {
        ChatRequest {
            model: "anthropic.claude-haiku-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            stream: Some(true),
            ..Default::default()
        }
    }

    #[test]
    fn body_includes_bedrock_anthropic_version_and_beta_flags() {
        let cfg = fake_cfg();
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        assert_eq!(body["anthropic_beta"], json!(["context-1m-2025-08-07"]),);
        assert_eq!(body["top_p"], json!(0.9));
        assert!(body.get("stream").is_none(), "stream should be stripped");
    }
}
