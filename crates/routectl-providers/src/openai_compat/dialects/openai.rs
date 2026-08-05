//! Vanilla OpenAI: `reasoning.effort` -> top-level `reasoning_effort`,
//! and o-series / GPT-5 drop sampling params (driven by `ModelProfile`).
//! Reasoning is not surfaced in the response body, so no response/chunk
//! lifting is needed.

use serde_json::Value;

use routectl_core::{ChatRequest, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::{drop_sampling_params, insert_reasoning_effort};
use crate::effort::clamp_effort_to_supported;
use crate::model_profile::profile_for;

/// Vanilla OpenAI reasoning dialect (see module docs).
pub struct OpenAiDialect;
/// Shared instance of [`OpenAiDialect`].
pub static OPENAI: OpenAiDialect = OpenAiDialect;

impl Dialect for OpenAiDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::OpenAi.format_tag()
    }

    fn apply_request(
        &self,
        _id: &str,
        obj: &mut serde_json::Map<String, Value>,
        req: &ChatRequest,
    ) -> Result<()> {
        if let Some(effort) = req.reasoning.as_ref().and_then(|r| r.effort.as_deref()) {
            // Vanilla OpenAI has no disable token, so omission is the disable
            // form -- a `None` clamp must never become a positive level.
            insert_reasoning_effort(
                obj,
                clamp_effort_to_supported(effort, &req.routectl_internal.effort_levels),
            );
        }
        if profile_for(&req.model).drops_sampling_params {
            drop_sampling_params(obj);
            // Real OpenAI rejects `max_tokens` on o-series / gpt-5 reasoning
            // models and requires `max_completion_tokens`. The OpenAI ingress
            // renames the inbound `max_completion_tokens` to canonical
            // `max_tokens`; restore it here for reasoning models only.
            if let Some(v) = obj.remove("max_tokens") {
                obj.insert("max_completion_tokens".into(), v);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

    use super::super::super::HistoryReasoning;
    use super::super::super::dialect::ReasoningDialect;
    use super::super::super::request::normalize;

    fn user_req(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        }
    }

    // Explicit effort passthrough: a positive level reaches the wire verbatim.
    #[test]
    fn explicit_effort_passes_through() {
        let mut req = user_req("gpt-5");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("low".into()),
            max_tokens: None,
            enabled: None,
            exclude: None,
        });

        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        assert_eq!(body["reasoning_effort"], "low");
    }

    // effort:"none" is reasoning-OFF: vanilla OpenAI has no disable token, so
    // reasoning_effort must be OMITTED -- never clamped to a positive level.
    #[test]
    fn none_effort_omits_reasoning_effort() {
        let mut req = user_req("gpt-5");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("none".into()),
            max_tokens: None,
            enabled: None,
            exclude: None,
        });

        let body = normalize(
            "test",
            &req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        assert!(
            body.get("reasoning_effort").is_none(),
            "effort:none must omit reasoning_effort, not emit a positive level: {body}"
        );
    }
}
