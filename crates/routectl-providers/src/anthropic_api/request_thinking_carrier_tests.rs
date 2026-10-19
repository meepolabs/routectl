//! The `thinking` an Anthropic-shape body carries, per ingress carrier
//! state: an Anthropic client's own object rides through as sent, changed
//! only by an explicit operator knob; a client that sent no thinking gets
//! none unless the adaptive knob synthesizes it; everything else is
//! translated from canonical reasoning. A forced tool choice strips only
//! active thinking.
//!
//! Requests are built the way the Anthropic ingress builds them (see
//! `ingress_request`), with the carrier set directly.

use routectl_core::wire_check::{EditKind, EditSite, Leg, WireTap};
use routectl_core::{
    AnthropicThinking, ChatRequest, Message, MessageContent, ReasoningConfig, Role, ThinkingObject,
};
use serde_json::{Value, json};

use super::normalize;

/// Build a request the way the Anthropic ingress does from `thinking` and
/// `output_config.effort`: the canonical reasoning it derives per thinking
/// type, the effort lift (with `output_config` kept in `provider_extras`),
/// the `display` carrier, and the thinking carrier itself.
pub fn ingress_request(
    thinking: Option<Value>,
    effort: Option<&str>,
    max_tokens: u32,
) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-test".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(max_tokens),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking = match thinking {
        Some(t) => {
            req.reasoning = canonical_reasoning(&t);
            req.routectl_internal.anthropic_thinking_display =
                t.get("display").and_then(Value::as_str).map(str::to_string);
            AnthropicThinking::Sent(ThinkingObject::from_client(t).expect("object"))
        }
        None => AnthropicThinking::SentNone,
    };
    if let Some(effort) = effort {
        req.reasoning
            .get_or_insert_with(ReasoningConfig::default)
            .effort = Some(effort.to_string());
        req.provider_extras = Some(json!({"output_config": {"effort": effort}}));
    }
    req
}

/// The canonical reasoning the ingress derives from a `thinking` object.
fn canonical_reasoning(t: &Value) -> Option<ReasoningConfig> {
    let display = t.get("display").and_then(Value::as_str);
    let exclude = match display {
        Some("omitted" | "updates") => Some(true),
        Some("summarized") => Some(false),
        _ => None,
    };
    let reasoning = match t.get("type").and_then(Value::as_str) {
        Some("enabled") => ReasoningConfig {
            enabled: Some(true),
            max_tokens: t
                .get("budget_tokens")
                .and_then(Value::as_u64)
                .map(|n| u32::try_from(n).unwrap_or(u32::MAX)),
            exclude,
            ..Default::default()
        },
        Some("disabled") => ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        },
        Some("adaptive") => ReasoningConfig {
            enabled: Some(true),
            exclude,
            ..Default::default()
        },
        _ => ReasoningConfig::default(),
    };
    Some(reasoning)
}

pub(super) fn body(req: &ChatRequest, adaptive: bool) -> Value {
    normalize("p", req, adaptive, false, None, false, true).expect("normalize")
}

fn assert_verbatim(sent: &Value, wire: &Value, row: &str) {
    assert_eq!(
        serde_json::to_string(wire).expect("serialize"),
        serde_json::to_string(sent).expect("serialize"),
        "{row}: the client's thinking must reach the wire as sent"
    );
}

fn driver_fixture(case: &str, file: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../routectl-cli/tests/fixtures/driver/anthropic-api")
        .join(case)
        .join(file);
    let text = std::fs::read_to_string(&path).expect("driver fixture must read");
    serde_json::from_str(&text).expect("driver fixture must parse")
}

/// The committed captures as positive controls: what the client sent is
/// what the upstream received. Both shapes are also ones the canonical
/// translation reproduces, so the carrier-only rows below are what pin the
/// forwarding path.
#[test]
fn committed_driver_captures_reach_the_wire_byte_identical() {
    for case in ["thinking-01", "plain-turn-01"] {
        // Arrange
        let ingress = driver_fixture(case, "ingress_request.json");
        let outgoing = driver_fixture(case, "outgoing_request.json");
        let effort = ingress
            .pointer("/output_config/effort")
            .and_then(Value::as_str);
        let max_tokens = ingress["max_tokens"].as_u64().expect("max_tokens") as u32;
        let req = ingress_request(Some(ingress["thinking"].clone()), effort, max_tokens);

        // Act
        let wire = body(&req, false);

        // Assert
        assert_verbatim(&ingress["thinking"], &wire["thinking"], case);
        assert_eq!(
            wire["thinking"], outgoing["thinking"],
            "{case}: committed egress"
        );
    }
}

/// Shapes the canonical translation cannot produce reach the wire as sent,
/// with or without the adaptive knob.
#[test]
fn client_thinking_shapes_forward_verbatim() {
    let rows = [
        (
            "adaptive",
            json!({"type": "adaptive", "display": "summarized"}),
            false,
        ),
        ("between_tools", json!({"type": "between_tools"}), false),
        (
            "between_tools, knob set",
            json!({"type": "between_tools"}),
            true,
        ),
        (
            "unknown type and key",
            json!({"type": "future_mode", "level": 3}),
            false,
        ),
        ("disabled, knob set", json!({"type": "disabled"}), true),
    ];
    for (row, thinking, adaptive) in rows {
        let req = ingress_request(Some(thinking.clone()), Some("high"), 64_000);

        let wire = body(&req, adaptive);

        assert_verbatim(&thinking, &wire["thinking"], row);
    }
}

#[test]
fn sent_none_emits_thinking_only_for_the_knob_with_a_positive_effort() {
    let rows: [(&str, Option<&str>, bool, Option<&str>); 5] = [
        ("knob unset, effort high", Some("high"), false, None),
        ("knob unset, no effort", None, false, None),
        (
            "knob set, effort high",
            Some("high"),
            true,
            Some("adaptive"),
        ),
        ("knob set, no effort", None, true, None),
        ("knob set, effort none", Some("none"), true, None),
    ];
    for (row, effort, adaptive, expected) in rows {
        let req = ingress_request(None, effort, 64_000);

        let wire = body(&req, adaptive);

        assert_eq!(
            wire.pointer("/thinking/type").and_then(Value::as_str),
            expected,
            "{row}: {wire}"
        );
        if expected.is_none() {
            assert!(wire.get("thinking").is_none(), "{row}: no thinking key");
        }
    }
}

#[test]
fn sent_none_with_the_knob_keeps_the_synthesized_adaptive_shape() {
    let req = ingress_request(None, Some("high"), 64_000);

    let wire = body(&req, true);

    assert_eq!(wire["thinking"], json!({"type": "adaptive"}));
    assert_eq!(wire["output_config"]["effort"], "high");
}

#[test]
fn the_adaptive_knob_turns_client_enabled_into_adaptive_keeping_display() {
    let thinking = json!({"type": "enabled", "budget_tokens": 8192, "display": "summarized"});
    let req = ingress_request(Some(thinking), Some("high"), 64_000);

    let wire = body(&req, true);

    assert_eq!(
        wire["thinking"],
        json!({"type": "adaptive", "display": "summarized"})
    );
    assert_eq!(wire["output_config"]["effort"], "high");
}

#[test]
fn an_unset_knob_forwards_client_adaptive_as_adaptive() {
    let thinking = json!({"type": "adaptive"});
    let req = ingress_request(Some(thinking.clone()), Some("medium"), 64_000);

    let wire = body(&req, false);

    assert_verbatim(&thinking, &wire["thinking"], "adaptive, knob unset");
    assert_eq!(wire["output_config"]["effort"], "medium");
}

#[test]
fn the_operator_budget_cap_lowers_only_a_larger_forwarded_enabled_budget() {
    let rows: [(&str, u32, u64); 4] = [
        ("cap below budget", 4096, 4096),
        ("cap zero", 0, 8192),
        ("cap above budget", 16_384, 8192),
        ("cap equal to budget", 8192, 8192),
    ];
    for (row, cap, expected) in rows {
        let thinking = json!({"type": "enabled", "budget_tokens": 8192});
        let mut req = ingress_request(Some(thinking), None, 64_000);
        req.routectl_internal.max_thinking_budget = cap;

        let wire = body(&req, false);

        assert_eq!(wire["thinking"]["type"], "enabled", "{row}");
        assert_eq!(wire["thinking"]["budget_tokens"], expected, "{row}");
    }
}

#[test]
fn a_sub_floor_budget_is_forwarded_on_sent_and_floored_on_translation() {
    // Arrange
    let sent = ingress_request(
        Some(json!({"type": "enabled", "budget_tokens": 500})),
        None,
        64_000,
    );
    let mut translated = sent.clone();
    translated.routectl_internal.anthropic_thinking = AnthropicThinking::NotCaptured;

    // Act
    let sent_wire = body(&sent, false);
    let translated_wire = body(&translated, false);

    // Assert
    assert_eq!(sent_wire["thinking"]["budget_tokens"], 500);
    assert_eq!(translated_wire["thinking"]["budget_tokens"], 1024);
}

#[test]
fn count_tokens_body_carries_the_forwarded_thinking() {
    let thinking = json!({"type": "between_tools"});
    let req = ingress_request(Some(thinking.clone()), None, 64_000);

    let count_body = crate::anthropic_api::build_count_tokens_body(&body(&req, false));

    assert_verbatim(&thinking, &count_body["thinking"], "count_tokens");
}

#[cfg(feature = "bedrock")]
#[test]
fn bedrock_invoke_body_carries_the_forwarded_thinking() {
    use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

    // Arrange
    let cfg = BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-test".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let thinking = json!({"type": "between_tools"});
    let req = ingress_request(Some(thinking.clone()), None, 64_000);

    // Act
    let wire = crate::bedrock::invoke::normalize_request(&cfg, &req).expect("invoke body");

    // Assert
    assert_verbatim(&thinking, &wire["thinking"], "bedrock invoke");
}

const CALLER_TEMPERATURE: f64 = 0.3;

fn forcing_tool_use(mut req: ChatRequest) -> ChatRequest {
    req.tools = Some(vec![routectl_core::ToolDef::Custom(
        routectl_core::CustomTool {
            name: "calc".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        },
    )]);
    req.tool_choice = Some(json!("required"));
    req
}

/// The body and the forced-tool strip declarations it made.
fn stripped(mut req: ChatRequest) -> (Value, Vec<(EditKind, String)>) {
    let tap = WireTap::admitted();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let wire = body(&req, false);
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .filter(|edit| edit.site() == EditSite::ThinkingStripForcedToolUse)
        .map(|edit| (edit.kind(), edit.path().to_string()))
        .collect();
    (wire, edits)
}

#[test]
fn forced_tool_use_strips_active_thinking_with_one_declaration() {
    for thinking in [
        json!({"type": "adaptive"}),
        json!({"type": "enabled", "budget_tokens": 4096}),
    ] {
        // Arrange
        let req = forcing_tool_use(ingress_request(
            Some(thinking.clone()),
            Some("high"),
            64_000,
        ));

        // Act
        let (wire, edits) = stripped(req);

        // Assert
        assert!(wire.get("thinking").is_none(), "{thinking}: {wire}");
        assert_eq!(wire["tool_choice"], json!({"type": "any"}), "{thinking}");
        assert_eq!(
            edits,
            vec![(EditKind::Remove, "/thinking".to_string())],
            "{thinking}"
        );
    }
}

/// Inactive and unknown thinking types spend no reasoning budget, so they
/// ride beside a forced tool choice with the caller's sampling, and nothing
/// is declared.
#[test]
fn forced_tool_use_keeps_inactive_thinking_and_the_caller_sampling() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode", "depth": 3}),
    ] {
        // Arrange
        let mut req = forcing_tool_use(ingress_request(
            Some(thinking.clone()),
            Some("high"),
            64_000,
        ));
        req.temperature = Some(CALLER_TEMPERATURE);

        // Act
        let (wire, edits) = stripped(req);

        // Assert
        assert_verbatim(&thinking, &wire["thinking"], "forced tool use");
        assert_eq!(
            wire["temperature"], CALLER_TEMPERATURE,
            "{thinking}: {wire}"
        );
        assert!(edits.is_empty(), "{thinking}: {edits:?}");
    }
}

/// `(row, thinking on the body, client effort, effort left on the body)`.
type StripEffortRow<'a> = (&'a str, Value, Option<&'a str>, Option<&'a str>);

/// The strip itself, on an assembled body: `output_config.effort` leaves
/// with stripped thinking only when routectl composed it. Called directly
/// because the effort reconcile that runs after it in assembly still drops
/// every effort that is not beside adaptive thinking.
#[test]
fn the_forced_tool_strip_drops_only_a_routectl_composed_effort() {
    let rows: &[StripEffortRow] = &[
        (
            "client effort beside stripped adaptive",
            json!({"type": "adaptive"}),
            Some("xhigh"),
            Some("high"),
        ),
        (
            "composed effort beside stripped adaptive",
            json!({"type": "adaptive"}),
            None,
            None,
        ),
        (
            "client effort beside kept disabled",
            json!({"type": "disabled"}),
            Some("xhigh"),
            Some("high"),
        ),
        (
            "composed effort beside kept between_tools",
            json!({"type": "between_tools"}),
            None,
            Some("high"),
        ),
    ];
    for (row, thinking, client_effort, expected) in rows {
        // Arrange
        let req = ingress_request(Some(thinking.clone()), *client_effort, 64_000);
        let mut wire = json!({
            "thinking": thinking,
            "tool_choice": {"type": "tool", "name": "calc"},
            "output_config": {"effort": "high"},
        });

        // Act
        crate::anthropic_api::extras::strip_thinking_when_tool_choice_forces_use(
            "p", &req, &mut wire, None,
        );

        // Assert
        assert_eq!(
            wire.pointer("/output_config/effort")
                .and_then(Value::as_str),
            *expected,
            "{row}: {wire}"
        );
    }
}
