//! DeepSeek: `reasoning_content` field on responses (lifted to
//! `reasoning_details[format=deepseek-v1]`); echoed assistant
//! `reasoning_content` in history triggers 400 (stripped on outgoing).
//! Reasoner variants drop sampling params (per `ModelProfile`).

use serde_json::Value;

use routectl_core::{ChatRequest, Message, Result};

use super::super::dialect::ReasoningDialect;
use super::util::{
    drop_sampling_params, lift_delta_reasoning_content, lift_reasoning_content_field,
    strip_history_reasoning,
};
use super::Dialect;
use crate::model_profile::profile_for;

pub struct DeepSeekDialect;
pub static DEEPSEEK: DeepSeekDialect = DeepSeekDialect;

impl Dialect for DeepSeekDialect {
    fn format_tag(&self) -> &'static str {
        ReasoningDialect::DeepSeek.format_tag()
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
        if profile_for(&req.model).drops_sampling_params {
            drop_sampling_params(obj);
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
