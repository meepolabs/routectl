//! Top-level request normalization. Walks a routectl `ChatRequest` into
//! the JSON body sent upstream:
//!
//!   1. Direct serde serialization of `ChatRequest`.
//!   2. Drop routectl-internal fields the upstream never wants.
//!   3. Hand off dialect-specific shaping to `Dialect::apply_request`.
//!   4. Merge `default_extras` then `provider_extras` last (callers win).
//!
//! Per-dialect logic lives in `dialects/*.rs`; this module is a thin
//! envelope around that dispatch.

use serde_json::Value;

use routectl_core::{ChatRequest, Error, Result};

use super::dialect::ReasoningDialect;

pub fn normalize(
    id: &str,
    req: &ChatRequest,
    dialect: ReasoningDialect,
    default_extras: Option<&Value>,
) -> Result<Value> {
    let mut body = serde_json::to_value(req)
        .map_err(|e| Error::normalize_request(id, e.to_string()))?;

    let obj = body.as_object_mut().ok_or_else(|| {
        Error::normalize_request(id, "serialized request is not an object")
    })?;

    // Remove routectl-internal fields that upstream never wants.
    // Dialects that need `chat_template_kwargs` re-inject it themselves.
    obj.remove("reasoning");
    obj.remove("provider_extras");
    obj.remove("chat_template_kwargs");

    dialect.as_dyn().apply_request(id, obj, req)?;

    // Merge default_extras first, then provider_extras (caller wins).
    if let Some(extras) = default_extras {
        merge_extras(obj, extras);
    }
    if let Some(extras) = req.provider_extras.as_ref() {
        merge_extras(obj, extras);
    }

    Ok(body)
}

/// Shallow-merge `extras` into `obj`. Extras keys win.
fn merge_extras(obj: &mut serde_json::Map<String, Value>, extras: &Value) {
    if let Some(extra_obj) = extras.as_object() {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
    use serde_json::json;

    fn simple_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(512),
            stop: None,
            stream: None,
            n: None,
            seed: None,
            logprobs: None,
            top_logprobs: None,
            logit_bias: None,
            presence_penalty: Some(0.1),
            frequency_penalty: Some(0.1),
            user: None,
            tools: None,
            tool_choice: None,
            response_format: None,
            reasoning: None,
            chat_template_kwargs: None,
            provider_extras: None,
        }
    }

    #[test]
    fn openai_passthrough_normal_model() {
        let req = simple_req("gpt-4o");
        let body = normalize("test", &req, ReasoningDialect::OpenAi, None).unwrap();
        // temperature preserved for non-reasoning models
        assert!(body.get("temperature").is_some());
        assert!(body.get("reasoning").is_none());
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn openai_drops_sampling_for_o_series() {
        let req = simple_req("o3-mini");
        let body = normalize("test", &req, ReasoningDialect::OpenAi, None).unwrap();
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("presence_penalty").is_none());
    }

    #[test]
    fn openai_maps_reasoning_effort() {
        let mut req = simple_req("o3");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let body = normalize("test", &req, ReasoningDialect::OpenAi, None).unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn deepseek_drops_sampling_for_reasoner() {
        let req = simple_req("deepseek-reasoner");
        let body = normalize("test", &req, ReasoningDialect::DeepSeek, None).unwrap();
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn deepseek_strips_reasoning_content_from_history() {
        let mut req = simple_req("deepseek-reasoner");
        req.messages.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text("I thought about it".into()),
            reasoning: Some("hidden chain".into()),
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        });
        let body = normalize("test", &req, ReasoningDialect::DeepSeek, None).unwrap();
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            assert!(m.get("reasoning_content").is_none());
            assert!(m.get("reasoning").is_none());
            assert!(m.get("reasoning_details").is_none());
        }
    }

    #[test]
    fn vllm_injects_enable_thinking() {
        let mut req = simple_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            enabled: Some(true),
            ..Default::default()
        });
        let body = normalize("test", &req, ReasoningDialect::Vllm, None).unwrap();
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    #[test]
    fn provider_extras_merged_last() {
        let mut req = simple_req("gpt-4o");
        req.provider_extras = Some(json!({"custom_key": "custom_val"}));
        let body = normalize("test", &req, ReasoningDialect::Passthrough, None).unwrap();
        assert_eq!(body["custom_key"], "custom_val");
        assert!(body.get("provider_extras").is_none());
    }

    #[test]
    fn default_extras_overridden_by_provider_extras() {
        let req_mut = {
            let mut r = simple_req("gpt-4o");
            r.provider_extras = Some(json!({"key": "from_request"}));
            r
        };
        let defaults = json!({"key": "from_defaults"});
        let body = normalize("test", &req_mut, ReasoningDialect::Passthrough, Some(&defaults)).unwrap();
        assert_eq!(body["key"], "from_request");
    }
}
