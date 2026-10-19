//! The sampling clamp and the public thinking-type read both key off the
//! EMITTED `thinking.type`, so they agree with the body for every carrier
//! state.
//!
//! The read reports the composed thinking. Three later assembly passes can
//! still remove or rewrite it, so their inputs stay out of the equality
//! table: a `tool_choice` that forces tool use, a context-management cache
//! miss, and an adaptive body whose `output_config.effort` is "none".

use routectl_core::{AnthropicThinking, ChatRequest, ReasoningConfig};
use serde_json::{Value, json};

use super::clamp_sampling_for_thinking;
use super::thinking_carrier_tests::{body, ingress_request};
use crate::anthropic_api::emitted_thinking_type;
use crate::anthropic_api::extras::reconcile_sampling_params;

const CALLER_TEMPERATURE: f64 = 0.5;

#[test]
fn the_clamp_pins_temperature_only_for_active_thinking_types() {
    let rows: [(&str, Option<&str>, Option<f64>); 6] = [
        ("enabled", Some("enabled"), Some(1.0)),
        ("adaptive", Some("adaptive"), Some(1.0)),
        ("disabled", Some("disabled"), Some(CALLER_TEMPERATURE)),
        (
            "between_tools",
            Some("between_tools"),
            Some(CALLER_TEMPERATURE),
        ),
        ("unknown", Some("future_mode"), Some(CALLER_TEMPERATURE)),
        ("no thinking", None, Some(CALLER_TEMPERATURE)),
    ];
    for (row, thinking_type, expected) in rows {
        let (temperature, top_p) =
            clamp_sampling_for_thinking(thinking_type, Some(CALLER_TEMPERATURE), Some(0.9));

        assert_eq!(temperature, expected, "{row}");
        assert_eq!(top_p, None, "{row}: temperature in play drops top_p");
    }
}

#[test]
fn the_body_keeps_caller_sampling_for_inactive_forwarded_types() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode"}),
    ] {
        let mut req = ingress_request(Some(thinking.clone()), None, 64_000);
        req.temperature = Some(CALLER_TEMPERATURE);

        let wire = body(&req, false);

        assert_eq!(wire["thinking"], thinking);
        assert_eq!(wire["temperature"], CALLER_TEMPERATURE, "{thinking}");
    }
}

/// The client's adaptive object rides to a lane whose knob is unset, on a
/// probe-sized request where the canonical translation would emit no
/// thinking at all. The body still carries adaptive thinking, so the clamp
/// must see the emitted type, not the translation.
#[test]
fn an_adaptive_carrier_on_a_non_adaptive_lane_pins_temperature() {
    // Arrange
    let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 512);
    req.temperature = Some(CALLER_TEMPERATURE);

    // Act
    let wire = body(&req, false);

    // Assert
    assert_eq!(wire["thinking"]["type"], "adaptive");
    assert_eq!(wire["temperature"], 1.0);
}

#[test]
fn the_sampling_reconciler_restores_caller_sampling_for_an_inactive_type() {
    let req = ChatRequest {
        temperature: Some(CALLER_TEMPERATURE),
        ..Default::default()
    };
    let mut wire = json!({"thinking": {"type": "between_tools"}, "temperature": 1.0});

    reconcile_sampling_params("p", &req, &mut wire);

    assert_eq!(wire["temperature"], CALLER_TEMPERATURE);
}

fn translated(reasoning: Option<ReasoningConfig>) -> ChatRequest {
    let mut req = ingress_request(None, None, 64_000);
    req.routectl_internal.anthropic_thinking = AnthropicThinking::NotCaptured;
    req.reasoning = reasoning;
    req
}

fn effort(level: &str) -> Option<ReasoningConfig> {
    Some(ReasoningConfig {
        effort: Some(level.into()),
        ..Default::default()
    })
}

#[test]
fn the_public_read_equals_the_emitted_thinking_type() {
    let sent = |t: Value, e: Option<&str>| ingress_request(Some(t), e, 64_000);
    let rows: Vec<(&str, ChatRequest, bool)> = vec![
        (
            "sent enabled",
            sent(json!({"type": "enabled", "budget_tokens": 8192}), None),
            false,
        ),
        (
            "sent enabled, knob",
            sent(
                json!({"type": "enabled", "budget_tokens": 8192}),
                Some("high"),
            ),
            true,
        ),
        (
            "sent adaptive",
            sent(json!({"type": "adaptive"}), Some("high")),
            false,
        ),
        (
            "sent disabled, effort",
            sent(json!({"type": "disabled"}), Some("high")),
            false,
        ),
        (
            "sent between_tools",
            sent(json!({"type": "between_tools"}), None),
            true,
        ),
        (
            "sent unknown",
            sent(json!({"type": "future_mode"}), None),
            false,
        ),
        (
            "sent no type",
            sent(json!({"budget_tokens": 2048}), None),
            false,
        ),
        (
            "sent none, effort",
            ingress_request(None, Some("high"), 64_000),
            false,
        ),
        (
            "sent none, effort, knob",
            ingress_request(None, Some("high"), 64_000),
            true,
        ),
        ("sent none, knob", ingress_request(None, None, 64_000), true),
        ("translated effort", translated(effort("high")), false),
        ("translated effort, knob", translated(effort("high")), true),
        ("translated effort none", translated(effort("none")), true),
        ("translated nothing", translated(None), false),
        (
            "translated probe-sized",
            {
                let mut r = translated(effort("high"));
                r.max_tokens = Some(64);
                r
            },
            false,
        ),
    ];
    let mut emitted_types = std::collections::BTreeSet::new();
    for (row, req, adaptive) in rows {
        let read = emitted_thinking_type(&req, adaptive);

        let wire = body(&req, adaptive);

        let on_wire = wire
            .pointer("/thinking/type")
            .and_then(Value::as_str)
            .map(str::to_string);
        assert_eq!(read, on_wire, "{row}: {wire}");
        emitted_types.insert(on_wire);
    }
    // The table spans every outcome the read can report.
    for expected in [
        None,
        Some("enabled"),
        Some("adaptive"),
        Some("disabled"),
        Some("between_tools"),
        Some("future_mode"),
    ] {
        assert!(
            emitted_types.contains(&expected.map(str::to_string)),
            "no row emits {expected:?}"
        );
    }
}
