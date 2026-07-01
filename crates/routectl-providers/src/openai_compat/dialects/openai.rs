//! Vanilla OpenAI: `reasoning.effort` -> top-level `reasoning_effort`,
//! and o-series / GPT-5 drop sampling params (driven by `ModelProfile`).
//! Reasoning is not surfaced in the response body, so no response/chunk
//! lifting is needed.

use serde_json::Value;

use routectl_core::{ChatRequest, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::drop_sampling_params;
use crate::effort::clamp_effort_to_supported;
use crate::model_profile::profile_for;

pub struct OpenAiDialect;
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
            let clamped = clamp_effort_to_supported(effort, &req.routectl_internal.effort_levels);
            obj.insert(
                "reasoning_effort".into(),
                Value::String(clamped.into_owned()),
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
