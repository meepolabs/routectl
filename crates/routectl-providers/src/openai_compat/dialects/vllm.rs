//! vLLM-served thinking models (Qwen3, MiMo, etc.):
//! `chat_template_kwargs.enable_thinking` enables reasoning;
//! `reasoning_content` field on response is lifted same as DeepSeek;
//! echoed history `reasoning_content` is stripped before sending.

use serde_json::{json, Value};

use routectl_core::{ChatRequest, Message, Result};

use super::super::dialect::ReasoningDialect;
use super::util::{
    lift_delta_reasoning_content, lift_reasoning_content_field, strip_history_reasoning,
};
use super::Dialect;

pub struct VllmDialect;
pub static VLLM: VllmDialect = VllmDialect;

impl Dialect for VllmDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::Vllm.format_tag()
    }

    fn strip_history_reasoning(&self) -> bool {
        true
    }

    fn lifts_reasoning_content(&self) -> bool {
        true
    }

    fn apply_request(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
        req: &ChatRequest,
    ) -> Result<()> {
        // Forward chat_template_kwargs if the caller supplied any;
        // otherwise auto-inject `enable_thinking` from the unified
        // reasoning config.
        if let Some(ctk) = req.chat_template_kwargs.as_ref() {
            obj.insert("chat_template_kwargs".into(), ctk.clone());
        } else if let Some(r) = req.reasoning.as_ref() {
            let enabled = r.enabled.unwrap_or(false);
            obj.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": enabled }),
            );
        }
        strip_history_reasoning(id, obj)?;
        Ok(())
    }

    fn apply_response(&self, _id: &str, msg: &mut Message) -> Result<()> {
        lift_reasoning_content_field(msg, self.format_tag());
        Ok(())
    }

    fn apply_chunk(&self, id: &str, val: &mut Value) -> Result<()> {
        lift_delta_reasoning_content(id, val, self.format_tag())
    }
}
