//! vLLM-served thinking models (Qwen3, MiMo, etc.):
//! `chat_template_kwargs.enable_thinking` enables reasoning;
//! `reasoning_content` field on response is lifted same as DeepSeek;
//! echoed history `reasoning_content` is stripped before sending.
//!
//! Reasoning effort derivation:
//! When the canonical request has `reasoning.effort` set, it is forwarded
//! verbatim as `reasoning_effort`. When only `reasoning.max_tokens` is
//! present, an effort string is derived from the budget:
//!   max_tokens < BUDGET_HIGH_THRESHOLD -> "medium"
//!   max_tokens >= BUDGET_HIGH_THRESHOLD -> "high"
//! Operator-supplied effort always wins over the derived value.

use serde_json::{json, Value};

use routectl_core::{ChatRequest, Message, Result};

use super::super::dialect::ReasoningDialect;
use super::util::{
    lift_delta_reasoning_content, lift_reasoning_content_field, preserve_history_reasoning_content,
};
use super::Dialect;
use crate::effort::clamp_effort_to_supported;

/// Budget threshold (tokens) above which `reasoning.max_tokens`
/// is mapped to "high" effort; below is "medium".
const BUDGET_HIGH_THRESHOLD: u32 = 8192;

/// Derive a `reasoning_effort` string from the canonical reasoning config.
/// Returns `Some(effort)` when the request carries any reasoning signal;
/// returns `None` when reasoning is absent or explicitly disabled.
///
/// Precedence: explicit `effort` > derived from `max_tokens` > None.
fn derive_reasoning_effort(req: &ChatRequest) -> Option<String> {
    let r = req.reasoning.as_ref()?;
    // Explicit effort wins over everything; passthrough verbatim.
    if let Some(effort) = r.effort.as_deref() {
        return Some(effort.to_string());
    }
    // Derive from max_tokens when effort is absent.
    if let Some(budget) = r.max_tokens {
        let effort = if budget >= BUDGET_HIGH_THRESHOLD {
            "high"
        } else {
            "medium"
        };
        return Some(effort.to_string());
    }
    None
}

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
        _id: &str,
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
        if let Some(effort) = derive_reasoning_effort(req) {
            let clamped = clamp_effort_to_supported(&effort, &req.routectl_internal.effort_levels);
            obj.insert(
                "reasoning_effort".into(),
                Value::String(clamped.into_owned()),
            );
        }
        // History-reasoning shaping (strip vs preserve) is owned by
        // the egress runtime; see DeepSeekDialect::apply_request for
        // the rationale.
        Ok(())
    }

    /// vLLM (recent versions) accepts the same `reasoning_content`
    /// preserve shape as DeepSeek for echo-back history.
    fn preserve_history_reasoning(
        &self,
        id: &str,
        obj: &mut serde_json::Map<String, Value>,
    ) -> Result<()> {
        preserve_history_reasoning_content(id, obj)
    }

    fn apply_response(&self, _id: &str, msg: &mut Message) -> Result<()> {
        lift_reasoning_content_field(msg, self.format_tag());
        Ok(())
    }

    fn apply_chunk(&self, id: &str, val: &mut Value) -> Result<()> {
        lift_delta_reasoning_content(id, val, self.format_tag())
    }
}

#[cfg(test)]
mod tests {
    use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

    use super::super::super::dialect::ReasoningDialect;
    use super::super::super::request::normalize;
    use super::super::super::HistoryReasoning;

    fn user_req(model: &str) -> ChatRequest {
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
            ..Default::default()
        }
    }

    // Explicit effort passthrough: operator-supplied effort travels verbatim.
    #[test]
    fn explicit_effort_passes_through() {
        // Arrange
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("low".into()),
            max_tokens: None,
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert: explicit effort "low" must reach the wire verbatim.
        assert_eq!(body["reasoning_effort"], "low");
    }

    // Operator-supplied effort wins when both effort and max_tokens are set.
    #[test]
    fn explicit_effort_wins_over_derived_from_max_tokens() {
        // Arrange: effort="low" + budget_tokens=16000 (would derive "high").
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("low".into()),
            max_tokens: Some(16000),
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert: "low" wins; budget_tokens alone would have produced "high".
        assert_eq!(
            body["reasoning_effort"], "low",
            "explicit effort must take precedence over derived value"
        );
    }

    // max_tokens below threshold derives "medium".
    #[test]
    fn max_tokens_below_threshold_derives_medium() {
        // Arrange: no effort, budget 4096 < 8192.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(4096),
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert
        assert_eq!(body["reasoning_effort"], "medium");
    }

    // max_tokens at the threshold derives "high".
    #[test]
    fn max_tokens_at_threshold_derives_high() {
        // Arrange: no effort, budget 8192 == BUDGET_HIGH_THRESHOLD.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(8192),
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert
        assert_eq!(body["reasoning_effort"], "high");
    }

    // max_tokens above threshold derives "high".
    #[test]
    fn max_tokens_above_threshold_derives_high() {
        // Arrange: no effort, budget 16000 > 8192.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(16000),
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert
        assert_eq!(body["reasoning_effort"], "high");
    }

    // No reasoning -> no reasoning_effort emitted.
    #[test]
    fn no_reasoning_emits_no_effort() {
        // Arrange: reasoning field absent entirely.
        let req = user_req("qwen3-30b");

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert: no reasoning -> no effort emitted, but
        // chat_template_kwargs must also be absent (no reasoning config
        // means no enable_thinking injection either).
        assert!(
            body.get("reasoning_effort").is_none(),
            "no reasoning config must not emit reasoning_effort: {body}"
        );
    }

    // enable_thinking is still injected alongside derived reasoning_effort.
    #[test]
    fn enable_thinking_and_derived_effort_coexist() {
        // Arrange: only max_tokens, no explicit effort.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(4096),
            enabled: Some(true),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false, true,
        )
        .unwrap();

        // Assert: both knobs are set; vLLM wants both on its thinking path.
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }
}
