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

use serde_json::{Value, json};

use routectl_core::{ChatRequest, Message, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::{
    derive_reasoning_effort, lift_delta_reasoning_content, lift_reasoning_content_field,
    preserve_history_reasoning_content, reasoning_enabled_for_wire,
};
use crate::effort::clamp_effort_to_supported;

/// vLLM-served thinking-model reasoning dialect (see module docs).
pub struct VllmDialect;
/// Shared instance of [`VllmDialect`].
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
        } else if req.reasoning.is_some() {
            obj.insert(
                "chat_template_kwargs".into(),
                json!({ "enable_thinking": reasoning_enabled_for_wire(req) }),
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

    fn apply_chunk(&self, id: &str, val: &mut Value, reasoning_index: &mut u32) -> Result<()> {
        lift_delta_reasoning_content(id, val, self.format_tag(), reasoning_index)
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
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
            false,
        )
        .unwrap();

        // Assert: both knobs are set; vLLM wants both on its thinking path.
        assert_eq!(body["reasoning_effort"], "medium");
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true);
    }

    // Reasoning explicitly disabled must NOT emit reasoning_effort,
    // even when an effort value is set (e.g. a model output_config default).
    // The enable_thinking flag and the effort gate must AGREE: both off.
    #[test]
    fn disabled_reasoning_emits_no_effort() {
        // Arrange: enabled=false but an effort is still present.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            enabled: Some(false),
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert: disabled reasoning suppresses the effort entirely, and
        // enable_thinking is off -- no contradictory payload.
        assert!(
            body.get("reasoning_effort").is_none(),
            "disabled reasoning must not emit reasoning_effort: {body}"
        );
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], false,
            "disabled reasoning must set enable_thinking=false: {body}"
        );
    }

    // enabled unset + explicit effort (what the OpenAI ingress produces when
    // promoting a top-level reasoning_effort) must turn thinking ON and
    // forward the effort -- the two must not disagree.
    #[test]
    fn enabled_none_with_effort_enables_thinking_and_emits_effort() {
        // Arrange: enabled unset, effort present.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            enabled: None,
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert: thinking on AND effort forwarded verbatim.
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], true,
            "effort-only request must enable thinking: {body}"
        );
        assert_eq!(body["reasoning_effort"], "high");
    }

    // enabled unset + max_tokens must turn thinking ON and derive the effort.
    #[test]
    fn enabled_none_with_max_tokens_enables_thinking_and_derives_effort() {
        // Arrange: enabled unset, only a budget present.
        let mut req = user_req("qwen3-30b");
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(4096),
            enabled: None,
            exclude: None,
        });

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::Vllm,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert: thinking on AND derived effort emitted.
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], true,
            "budget-only request must enable thinking: {body}"
        );
        assert_eq!(body["reasoning_effort"], "medium");
    }

    // Caller-supplied chat_template_kwargs wins: the unified reasoning config
    // must not overwrite an explicit caller payload.
    #[test]
    fn caller_supplied_chat_template_kwargs_wins() {
        // Arrange: caller sets chat_template_kwargs explicitly AND a reasoning
        // config that would otherwise inject enable_thinking=true.
        let mut req = user_req("qwen3-30b");
        req.chat_template_kwargs = Some(serde_json::json!({ "enable_thinking": false }));
        req.reasoning = Some(ReasoningConfig {
            effort: Some("high".into()),
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
            false,
        )
        .unwrap();

        // Assert: caller payload wins verbatim.
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"], false,
            "caller-supplied chat_template_kwargs must win: {body}"
        );
    }
}
