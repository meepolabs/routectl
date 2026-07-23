//! DeepSeek: `reasoning_content` field on responses (lifted to
//! `reasoning_details[format=deepseek-v1]`); echoed assistant
//! `reasoning_content` in history triggers 400 (stripped on outgoing).
//! Reasoner variants drop sampling params (per `ModelProfile`).
//!
//! Reasoning effort derivation:
//! When the canonical request has `reasoning.effort` set, it is forwarded
//! verbatim as `reasoning_effort`. When only `reasoning.max_tokens` is
//! present, an effort string is derived from the budget:
//!   max_tokens < BUDGET_HIGH_THRESHOLD -> "medium"
//!   max_tokens >= BUDGET_HIGH_THRESHOLD -> "high"
//! Operator-supplied effort always wins over the derived value.

use serde_json::Value;

use routectl_core::{ChatRequest, Message, Result};

use super::super::dialect::ReasoningDialect;
use super::Dialect;
use super::util::{
    derive_reasoning_effort, drop_sampling_params, lift_delta_reasoning_content,
    lift_reasoning_content_field, preserve_history_reasoning_content,
};
use crate::effort::clamp_effort_to_supported;
use crate::model_profile::profile_for;

/// DeepSeek reasoning dialect (see module docs).
pub struct DeepSeekDialect;
/// Shared instance of [`DeepSeekDialect`].
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
        _id: &str,
        obj: &mut serde_json::Map<String, Value>,
        req: &ChatRequest,
    ) -> Result<()> {
        if profile_for(&req.model).drops_sampling_params {
            drop_sampling_params(obj);
        }
        if let Some(effort) = derive_reasoning_effort(req) {
            let clamped = clamp_effort_to_supported(&effort, &req.routectl_internal.effort_levels);
            obj.insert(
                "reasoning_effort".into(),
                Value::String(clamped.into_owned()),
            );
        }
        // History-reasoning shaping (strip vs preserve) is owned by
        // the egress runtime in `request::normalize`, gated by the
        // provider's `history_reasoning` knob. Do NOT call
        // strip/preserve helpers here -- if we did, the runtime's
        // preserve-mode rewrite would be undone.
        Ok(())
    }

    /// Preserve outgoing assistant reasoning as `reasoning_content`
    /// on the wire. Required by DeepSeek v4+ which 400s with
    /// `"reasoning_content in the thinking mode must be passed back
    /// to the API"` if echo-back is missing.
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
            }],
            ..Default::default()
        }
    }

    // Explicit effort passthrough: operator-supplied effort travels verbatim.
    #[test]
    fn explicit_effort_passes_through() {
        // Arrange
        let mut req = user_req("deepseek-reasoner");
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
            ReasoningDialect::DeepSeek,
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
        let mut req = user_req("deepseek-reasoner");
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
            ReasoningDialect::DeepSeek,
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
        let mut req = user_req("deepseek-chat");
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
            ReasoningDialect::DeepSeek,
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
        let mut req = user_req("deepseek-chat");
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
            ReasoningDialect::DeepSeek,
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
        let mut req = user_req("deepseek-chat");
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
            ReasoningDialect::DeepSeek,
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
        let req = user_req("deepseek-chat");

        // Act
        let body = normalize(
            "test",
            &req,
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert
        assert!(
            body.get("reasoning_effort").is_none(),
            "no reasoning config must not emit reasoning_effort: {body}"
        );
    }

    // Reasoning explicitly disabled must NOT emit reasoning_effort,
    // even when an effort value is set (e.g. a model output_config default).
    #[test]
    fn disabled_reasoning_emits_no_effort() {
        // Arrange: enabled=false but an effort is still present.
        let mut req = user_req("deepseek-chat");
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
            ReasoningDialect::DeepSeek,
            HistoryReasoning::Auto,
            None,
            false,
        )
        .unwrap();

        // Assert: disabled reasoning suppresses the effort entirely.
        assert!(
            body.get("reasoning_effort").is_none(),
            "disabled reasoning must not emit reasoning_effort: {body}"
        );
    }
}
