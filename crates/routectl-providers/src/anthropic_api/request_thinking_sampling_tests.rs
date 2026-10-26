//! The sampling clamp and the public thinking-type read both key off the
//! EMITTED `thinking.type`, so they agree with the body for every carrier
//! state.
//!
//! The read and body assembly share one computation of the shipped thinking.
//! Its one input the read cannot see, a context-management cache miss, has
//! its own row showing the read over-reports there and nowhere else.

use std::num::NonZeroUsize;
use std::sync::RwLock;

use routectl_core::{AnthropicThinking, ChatRequest, ReasoningConfig, ToolDef};
use serde_json::{Value, json};

use super::clamp_sampling_for_thinking;
use super::thinking_carrier_tests::{body, ingress_request};
use crate::anthropic_api::emitted_thinking_type;

const CALLER_TEMPERATURE: f64 = 0.5;

type ClampRow = (
    &'static str,
    Option<&'static str>,
    Option<f64>,
    Option<f64>,
    (Option<f64>, Option<f64>),
);

#[test]
fn the_clamp_rewrites_only_a_client_temperature_under_active_thinking() {
    let rows: [ClampRow; 14] = [
        (
            "enabled, both",
            Some("enabled"),
            Some(0.3),
            Some(0.9),
            (Some(1.0), None),
        ),
        (
            "adaptive, both",
            Some("adaptive"),
            Some(0.3),
            Some(0.9),
            (Some(1.0), None),
        ),
        (
            "enabled, temperature",
            Some("enabled"),
            Some(0.3),
            None,
            (Some(1.0), None),
        ),
        (
            "adaptive, temperature",
            Some("adaptive"),
            Some(0.3),
            None,
            (Some(1.0), None),
        ),
        (
            "enabled, nothing",
            Some("enabled"),
            None,
            None,
            (None, None),
        ),
        (
            "adaptive, nothing",
            Some("adaptive"),
            None,
            None,
            (None, None),
        ),
        (
            "enabled, top_p",
            Some("enabled"),
            None,
            Some(0.9),
            (None, None),
        ),
        (
            "adaptive, top_p",
            Some("adaptive"),
            None,
            Some(0.9),
            (None, None),
        ),
        (
            "disabled, both",
            Some("disabled"),
            Some(0.3),
            Some(0.9),
            (Some(0.3), None),
        ),
        (
            "between_tools, both",
            Some("between_tools"),
            Some(0.3),
            Some(0.9),
            (Some(0.3), None),
        ),
        (
            "unknown, both",
            Some("future_mode"),
            Some(0.3),
            Some(0.9),
            (Some(0.3), None),
        ),
        (
            "no thinking, both",
            None,
            Some(0.3),
            Some(0.9),
            (Some(0.3), None),
        ),
        (
            "no thinking, top_p",
            None,
            None,
            Some(0.9),
            (None, Some(0.9)),
        ),
        ("no thinking, nothing", None, None, None, (None, None)),
    ];
    for (row, thinking_type, temperature, top_p, expected) in rows {
        let clamped = clamp_sampling_for_thinking(thinking_type, temperature, top_p);

        assert_eq!(clamped, expected, "{row}");
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

        let wire = body(&req);

        assert_eq!(wire["thinking"], thinking);
        assert_eq!(wire["temperature"], CALLER_TEMPERATURE, "{thinking}");
    }
}

/// The client's adaptive object rides on a probe-sized request where the canonical translation would emit no
/// thinking at all. The body still carries adaptive thinking, so the clamp
/// must see the emitted type, not the translation.
#[test]
fn an_adaptive_carrier_on_a_non_adaptive_lane_pins_temperature() {
    // Arrange
    let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 512);
    req.temperature = Some(CALLER_TEMPERATURE);

    // Act
    let wire = body(&req);

    // Assert
    assert_eq!(wire["thinking"]["type"], "adaptive");
    assert_eq!(wire["temperature"], 1.0);
}

#[test]
fn active_thinking_without_a_client_temperature_ships_no_temperature() {
    for thinking in [
        json!({"type": "adaptive"}),
        json!({"type": "enabled", "budget_tokens": 4096}),
    ] {
        // Arrange
        let req = ingress_request(Some(thinking.clone()), Some("high"), 64_000);

        // Act
        let wire = body(&req);

        // Assert
        assert_eq!(wire["thinking"]["type"], thinking["type"], "{wire}");
        assert!(wire.get("temperature").is_none(), "{wire}");
        assert!(wire.get("top_p").is_none(), "{wire}");
    }
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
    let rows: Vec<(&str, ChatRequest)> = vec![
        (
            "sent enabled",
            sent(json!({"type": "enabled", "budget_tokens": 8192}), None),
        ),
        (
            "sent adaptive",
            sent(json!({"type": "adaptive"}), Some("high")),
        ),
        (
            "sent disabled, effort",
            sent(json!({"type": "disabled"}), Some("high")),
        ),
        (
            "sent between_tools",
            sent(json!({"type": "between_tools"}), None),
        ),
        ("sent unknown", sent(json!({"type": "future_mode"}), None)),
        ("sent no type", sent(json!({"budget_tokens": 2048}), None)),
        (
            "sent none, effort",
            ingress_request(None, Some("high"), 64_000),
        ),
        ("sent none", ingress_request(None, None, 64_000)),
        ("translated effort", translated(effort("high"))),
        ("translated effort none", translated(effort("none"))),
        (
            "sent adaptive, effort none",
            sent(json!({"type": "adaptive"}), Some("none")),
        ),
        ("translated nothing", translated(None)),
        ("translated probe-sized", {
            let mut r = translated(effort("high"));
            r.max_tokens = Some(64);
            r
        }),
    ];
    let rows = rows.into_iter().chain(forced_tool_rows());
    let mut emitted_types = std::collections::BTreeSet::new();
    for (row, req) in rows {
        let read = emitted_thinking_type(&req);

        let wire = body(&req);

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

fn forcing(mut req: ChatRequest, tool_choice: Value) -> ChatRequest {
    req.tools = Some(vec![ToolDef::Custom(routectl_core::CustomTool {
        name: "calc".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);
    req.tool_choice = Some(tool_choice);
    req
}

/// Forced and unforced tool choices beside active and inactive thinking
/// types, for the equality table.
fn forced_tool_rows() -> Vec<(&'static str, ChatRequest)> {
    let sent = |t: Value| ingress_request(Some(t), Some("high"), 64_000);
    vec![
        (
            "forced any, sent adaptive",
            forcing(sent(json!({"type": "adaptive"})), json!("required")),
        ),
        (
            "forced tool, sent enabled",
            forcing(
                sent(json!({"type": "enabled", "budget_tokens": 4096})),
                json!({"type": "tool", "name": "calc"}),
            ),
        ),
        (
            "forced function, sent enabled",
            forcing(
                sent(json!({"type": "enabled", "budget_tokens": 4096})),
                json!({"type": "function", "function": {"name": "calc"}}),
            ),
        ),
        (
            "forced bare name, translated effort",
            forcing(translated(effort("high")), json!("calc")),
        ),
        (
            "forced any, sent disabled",
            forcing(sent(json!({"type": "disabled"})), json!({"type": "any"})),
        ),
        (
            "forced any, sent between_tools",
            forcing(sent(json!({"type": "between_tools"})), json!("required")),
        ),
        (
            "forced any, sent unknown",
            forcing(sent(json!({"type": "future_mode"})), json!("required")),
        ),
        (
            "auto, sent adaptive",
            forcing(sent(json!({"type": "adaptive"})), json!("auto")),
        ),
    ]
}

/// The read's one documented residual: a context-management cache miss is
/// known only while the body is assembled, so the body drops thinking the
/// read still reports. Only removal is possible there, never a different
/// type.
#[test]
fn a_context_cache_miss_is_the_one_case_the_read_over_reports() {
    // Arrange
    let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 64_000);
    req.provider_extras = Some(json!({
        "output_config": {"effort": "high"},
        "context_management": {"edits": [{"type": "clear_thinking_20251015", "keep": "all"}]},
    }));
    req.messages = vec![
        routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("use calc".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
        routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::Assistant,
            content: routectl_core::MessageContent::Text(String::new()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "toolu_t1",
                "type": "function",
                "function": {"name": "calc", "arguments": "{}"},
            })]),
        },
        routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::Tool,
            content: routectl_core::MessageContent::Text("42".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some("toolu_t1".into()),
            tool_calls: None,
        },
    ]
    .into();
    let empty_cache = RwLock::new(lru::LruCache::new(NonZeroUsize::new(8).expect("cap")));

    // Act
    let read = emitted_thinking_type(&req);
    let wire =
        super::normalize("p", &req, true, Some(&empty_cache), false, true).expect("normalize");

    // Assert
    assert_eq!(read.as_deref(), Some("adaptive"));
    assert!(wire.get("thinking").is_none(), "{wire}");
}
