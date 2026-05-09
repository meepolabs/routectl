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

    // Merge any additional model request fields at the top level
    // with the same routectl-managed-keys allow-list as the Anthropic
    // and openai-compat egresses. The Bedrock-Invoke body is in
    // Anthropic shape (today); without this filter, a config-file
    // `additional_model_request_fields = { "messages" = [...] }`
    // would silently replace the assembled history. Filter here
    // (rather than at config load) so the WARN line carries the
    // provider id at request time and matches the operator's
    // existing log workflow.
    if let Some(extras) = cfg.additional_model_request_fields.as_ref() {
        if let Some(extras_obj) = extras.as_object() {
            for (k, v) in extras_obj {
                if is_bedrock_invoke_managed_key(k) {
                    tracing::warn!(
                        provider = %cfg.id,
                        key = %k,
                        "additional_model_request_fields attempted to override \
                         routectl-managed key; dropped"
                    );
                    continue;
                }
                obj.insert(k.clone(), v.clone());
            }
        }
    }

    // Stream flag is decided at HTTP level (different endpoint suffix),
    // not via a body field, so strip any leftover.
    obj.remove("stream");

    // Bedrock's InvokeModel takes the model identifier in the URL path
    // (`/model/{model_id}/invoke`), not the body. The Anthropic API
    // path requires it in the body, which `anthropic_api::request::
    // normalize` honors -- but Bedrock's strict-schema validator
    // rejects a body-side `model` with "Extra inputs are not
    // permitted". Strip it here on the Bedrock-Invoke seam.
    obj.remove("model");

    Ok(body)
}

/// Top-level Bedrock-Invoke body keys (Anthropic-shape today) that
/// `additional_model_request_fields` is NOT permitted to override.
/// Mirrors the Anthropic egress allow-list plus the Bedrock-specific
/// `anthropic_version` field. Long-tail Anthropic-only knobs
/// (`top_k`, `metadata`, `service_tier`) still pass through.
fn is_bedrock_invoke_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "system"
            | "max_tokens"
            | "thinking"
            | "tools"
            | "tool_choice"
            | "stream"
            | "stop_sequences"
            | "temperature"
            | "top_p"
            | "anthropic_beta"
            | "anthropic_version"
            | "cache_control"
    )
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
            // `top_p` is canonical and would now be filtered out;
            // use `top_k` here as a real long-tail Anthropic-only
            // knob the allow-list lets through.
            additional_model_request_fields: Some(json!({"top_k": 40})),
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
        assert_eq!(body["top_k"], json!(40));
        assert!(body.get("stream").is_none(), "stream should be stripped");
        assert!(
            body.get("model").is_none(),
            "model must be stripped: Bedrock takes it in the URL, not the body"
        );
    }

    #[test]
    fn additional_model_request_fields_cannot_override_managed_keys() {
        // Regression for the round 6 finding: a misconfigured
        // `additional_model_request_fields = { "messages" = [...] }`
        // would silently replace the assembled history. Now blocked
        // with a WARN, matching the openai-compat / anthropic_api
        // egress filters.
        let mut cfg = fake_cfg();
        cfg.additional_model_request_fields = Some(json!({
            // managed -- MUST be dropped:
            "messages": [{"role": "user", "content": "INJECTED"}],
            "system": "INJECTED",
            "anthropic_version": "evil-version",
            "max_tokens": 1,
            // long-tail -- MUST pass through:
            "metadata": {"user_id": "u-1"},
        }));
        let req = user_req();
        let body = normalize_request(&cfg, &req).unwrap();
        // Anthropic-version stays as the Bedrock-required value.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
        // Messages stays the user_req() text, NOT "INJECTED".
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_ne!(messages[0]["content"], "INJECTED");
        // System didn't get stomped.
        assert!(body.get("system").is_none() || body["system"] != "INJECTED");
        // max_tokens stays the request's value, not the override.
        assert_ne!(body["max_tokens"], 1);
        // Long-tail extras land verbatim.
        assert_eq!(body["metadata"]["user_id"], "u-1");
    }

    #[test]
    fn cache_control_on_user_text_round_trips_to_bedrock_invoke_body() {
        // v0.4.0 mandate: Anthropic-in -> Bedrock-Invoke-out path
        // preserves cache_control byte-for-byte. Bedrock-Invoke
        // delegates body construction to anthropic_api::request::
        // normalize, so this test pins the inheritance.
        use routectl_core::{
            cache_control::CacheControl, content_part::ContentPart, system_content::SystemContent,
            KnownContentPart, SystemBlock,
        };

        let cfg = fake_cfg();
        let mut req = user_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "be helpful".into(),
            cache_control: Some(CacheControl::ephemeral_1h()),
            citations: None,
        }]));
        req.messages[0].content =
            MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "look".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]);

        let body = normalize_request(&cfg, &req).unwrap();

        // Top-level cache_control on body.
        assert_eq!(body["cache_control"]["ttl"], "5m");
        // System block preserved with cache_control.
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys[0]["cache_control"]["ttl"], "1h");
        // User text block preserved with cache_control.
        let blk = &body["messages"][0]["content"][0];
        assert_eq!(blk["cache_control"]["ttl"], "5m");
        // Bedrock-required version still set.
        assert_eq!(body["anthropic_version"], json!("bedrock-2023-05-31"));
    }
}
