//! Per-dialect request normalization.
//!
//! Converts a routectl `ChatRequest` into the JSON body sent upstream.
//! Rules:
//!   - Start from a direct serde serialization of `ChatRequest`.
//!   - Strip fields unsupported by the dialect/model.
//!   - Inject dialect-specific reasoning params.
//!   - Strip `reasoning_content` / `reasoning_details` from message history
//!     for dialects that 400 on them.
//!   - Merge `default_extras` then `provider_extras` last (callers win).
//!   - Remove `chat_template_kwargs` unless the dialect uses it.

use serde_json::{json, Value};

use routectl_core::{ChatRequest, Error, Result};

use super::dialect::ReasoningDialect;

/// Fields that o-series / reasoning-only models do not accept.
const OPENAI_REASONING_DROP: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
];

/// Same set for DeepSeek reasoner and vLLM thinking models.
const DEEPSEEK_REASONING_DROP: &[&str] = &[
    "temperature",
    "top_p",
    "presence_penalty",
    "frequency_penalty",
    "logprobs",
    "top_logprobs",
];

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
    obj.remove("reasoning");
    obj.remove("provider_extras");
    obj.remove("chat_template_kwargs");

    apply_dialect(id, obj, req, dialect)?;

    // Merge default_extras first, then provider_extras (caller wins).
    if let Some(extras) = default_extras {
        merge_extras(obj, extras);
    }
    if let Some(extras) = req.provider_extras.as_ref() {
        merge_extras(obj, extras);
    }

    Ok(body)
}

fn apply_dialect(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
    dialect: ReasoningDialect,
) -> Result<()> {
    match dialect {
        ReasoningDialect::OpenAi => apply_openai(obj, req),
        ReasoningDialect::DeepSeek => apply_deepseek(id, obj, req)?,
        ReasoningDialect::Vllm => apply_vllm(id, obj, req)?,
        ReasoningDialect::RawThinkTag => {}
        ReasoningDialect::OpenRouter => {}
        ReasoningDialect::Passthrough => {}
    }
    Ok(())
}

fn apply_openai(obj: &mut serde_json::Map<String, Value>, req: &ChatRequest) {
    // Translate `reasoning.effort` -> top-level `reasoning_effort`.
    if let Some(effort) = req.reasoning.as_ref().and_then(|r| r.effort.as_deref()) {
        obj.insert("reasoning_effort".into(), Value::String(effort.into()));
    }

    // o1/o3/o4/gpt-5 do not accept sampling params.
    if is_openai_reasoning_model(&req.model) {
        for key in OPENAI_REASONING_DROP {
            obj.remove(*key);
        }
    }
}

fn apply_deepseek(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
) -> Result<()> {
    // Reasoning-specific model variants reject sampling params.
    if req.model.contains("reasoner") {
        for key in DEEPSEEK_REASONING_DROP {
            obj.remove(*key);
        }
    }

    strip_history_reasoning(id, obj, req)?;
    Ok(())
}

fn apply_vllm(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
) -> Result<()> {
    // Forward `chat_template_kwargs` if present -- vLLM accepts them.
    if let Some(ctk) = req.chat_template_kwargs.as_ref() {
        obj.insert("chat_template_kwargs".into(), ctk.clone());
    } else if let Some(r) = req.reasoning.as_ref() {
        // Auto-inject `enable_thinking` from the unified reasoning config.
        let enabled = r.enabled.unwrap_or(false);
        obj.insert(
            "chat_template_kwargs".into(),
            json!({"enable_thinking": enabled}),
        );
    }

    strip_history_reasoning(id, obj, req)?;
    Ok(())
}

/// Remove `reasoning_content` and `reasoning_details` from every assistant
/// message in the outgoing body. DeepSeek and vLLM 400 on those fields.
fn strip_history_reasoning(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    _req: &ChatRequest,
) -> Result<()> {
    let messages = obj
        .get_mut("messages")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| Error::normalize_request(id, "messages is not an array"))?;

    for msg in messages.iter_mut() {
        if let Some(m) = msg.as_object_mut() {
            m.remove("reasoning_content");
            m.remove("reasoning_details");
            m.remove("reasoning");
        }
    }
    Ok(())
}

/// Shallow-merge `extras` into `obj`. Extras keys win.
fn merge_extras(obj: &mut serde_json::Map<String, Value>, extras: &Value) {
    if let Some(extra_obj) = extras.as_object() {
        for (k, v) in extra_obj {
            obj.insert(k.clone(), v.clone());
        }
    }
}

fn is_openai_reasoning_model(model: &str) -> bool {
    let lower = model.to_lowercase();
    lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("gpt-5")
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
