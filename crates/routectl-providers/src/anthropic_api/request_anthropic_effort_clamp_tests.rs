use super::*;
use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};

fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// Operator declares effort_levels = ["low","medium","high"] on an
/// Anthropic adaptive model. Caller sends effort="max". The outgoing
/// output_config.effort must be "high" (clamped down to the operator
/// cap), not "max".
#[test]
fn adaptive_clamps_effort_to_operator_cap() {
    // Arrange
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ]);

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert: effort clamped from "max" down to "high" (operator cap).
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "high",
        "effort must clamp from max to high against operator-declared effort_levels; got: {oc}"
    );
}

/// Operator declares effort_levels = [] (empty). Caller sends
/// effort="max". The outgoing output_config.effort must be "max"
/// (pass-through; current Anthropic behavior).
#[test]
fn adaptive_passthrough_when_effort_levels_empty() {
    // Arrange
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    // Empty = pass-through semantics (default).
    req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert: effort passes through unchanged.
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "max",
        "empty effort_levels must not clamp; got: {oc}"
    );
}

/// Operator declares effort_levels = ["low","medium"] on an
/// Anthropic legacy (non-adaptive) model. Caller sends effort="high".
/// The legacy budget must be derived from "medium" (clamped down to
/// the operator cap), not "high".
///
/// Concretely: max_tokens=4096. The exact table maps "medium" to
/// 8192, which the `[1024, max_tokens-1]` window then clamps to
/// 4095. The high band (24576) would clamp to the same ceiling, so
/// the cost cap is observed at the table-lookup layer: this test
/// pins that effort is clamped to "medium" before the budget lookup.
#[test]
fn legacy_clamps_effort_to_operator_cost_cap() {
    // Arrange
    let mut req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(4096),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.effort_levels =
        std::sync::Arc::from(vec!["low".to_string(), "medium".to_string()]);

    // Act
    let body =
        normalize("test", &req, false, false, None, false, true).expect("normalize must succeed");

    // Assert: "medium" table budget 8192 window-clamped to 4095.
    let thinking = body.get("thinking").expect("thinking field present");
    assert_eq!(thinking["type"], "enabled");
    assert_eq!(
        thinking["budget_tokens"], 4095,
        "legacy path must clamp effort from high to medium against operator cap; got: {thinking}"
    );
}

/// Companion to `adaptive_clamps_effort_to_operator_cap`: the clamp
/// must hold even when the caller's raw `output_config.effort`
/// arrives via `provider_extras`. claude-code 2.1.153+ sends
/// `output_config: {effort: "max"}` on every request; the Anthropic
/// ingress preserves the whole `output_config` object verbatim in
/// `provider_extras` so the orthogonal `output_config.format`
/// sub-key (structured-output) passes through. build_output_config
/// clamps "max" -> "high" on the typed struct, but merge_provider_extras
/// then overwrites the clamped wire value with the raw caller
/// value. Without the re-clamp in reconcile_output_config_effort, the
/// operator's effort_levels cap is silently bypassed.
///
/// The pre-existing `adaptive_clamps_effort_to_operator_cap` test
/// leaves `provider_extras=None` so `merge_provider_extras` early-
/// returns and the bug is masked; the
/// `output_config_effort_preserved_on_adaptive_provider` test has
/// empty `effort_levels` so there is no cap to violate. This test
/// pins both: non-empty `effort_levels` AND raw `output_config.effort`
/// in `provider_extras`.
#[test]
fn adaptive_clamps_effort_to_operator_cap_even_when_provider_extras_carries_raw() {
    use serde_json::json;

    // Arrange: caller asks for effort="max" both via the canonical
    // lift (req.reasoning) and via the raw output_config that the
    // ingress mirrored into provider_extras (claude-code shape);
    // operator caps effort_levels at "high".
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {"effort": "max"}
        })),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ]);

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert: effort clamped to "high" even though raw "max" was
    // layered back in by merge_provider_extras.
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "high",
        "effort_levels cap (high) must override caller-supplied output_config.effort=max \
         even when carried via provider_extras; got: {oc}"
    );
}

/// Companion: empty effort_levels = intentional pass-through, no
/// re-clamp. Even when provider_extras carries
/// `output_config.effort = "max"`, an operator who declared
/// `effort_levels = []` (or omitted it) wants the raw value to flow
/// through verbatim.
#[test]
fn adaptive_passes_through_provider_extras_effort_when_levels_empty() {
    use serde_json::json;

    // Arrange
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {"effort": "max"}
        })),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(Vec::<String>::new());

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "max",
        "empty effort_levels must pass provider_extras output_config.effort through unchanged; got: {oc}"
    );
}

/// Companion: `output_config.format` (structured-output) and other
/// sibling sub-keys inside `output_config` must continue to flow
/// through verbatim from provider_extras. The re-clamp must only
/// touch the `effort` sub-key, never `format`.
#[test]
fn adaptive_reclamp_preserves_sibling_output_config_keys() {
    use serde_json::json;

    // Arrange
    let mut req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {
                "effort": "max",
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object", "required": ["x"]}
                }
            }
        })),
        ..Default::default()
    };
    req.routectl_internal.effort_levels = std::sync::Arc::from(vec![
        "low".to_string(),
        "medium".to_string(),
        "high".to_string(),
    ]);

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert: effort clamped, format preserved verbatim.
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(oc["effort"], "high", "effort must clamp; got: {oc}");
    assert_eq!(oc["format"]["type"], "json_schema");
    assert_eq!(oc["format"]["schema"]["required"][0], "x");
}

/// A sent `output_config.effort` of "none" beside adaptive thinking ships as
/// sent: routectl does not rewrite the pairing into the disabled shape, the
/// upstream judges it.
#[test]
fn adaptive_provider_extras_none_effort_ships_as_sent() {
    use serde_json::json;

    // Arrange: canonical effort "max" (adaptive), provider_extras override "none".
    let req = ChatRequest {
        model: "claude-opus-4-7".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(1024),
        reasoning: Some(ReasoningConfig {
            effort: Some("max".into()),
            max_tokens: None,
            exclude: None,
            enabled: Some(true),
        }),
        provider_extras: Some(json!({
            "output_config": {"effort": "none"}
        })),
        ..Default::default()
    };

    // Act
    let body =
        normalize("test", &req, true, false, None, false, true).expect("normalize must succeed");

    // Assert
    assert_eq!(body["output_config"]["effort"], "none", "{body}");
    assert_eq!(body["thinking"]["type"], "adaptive", "{body}");
}

#[test]
fn output_config_is_not_routectl_managed() {
    // Pinning this invariant: output_config must remain a non-managed
    // key so provider_extras-carried sub-fields like
    // `output_config.format` flow through verbatim. The adaptive-branch
    // re-clamp at reconcile_output_config_effort relies on output_config
    // surviving merge_provider_extras intact.
    assert!(!crate::managed_keys::is_managed_key(
        crate::managed_keys::Dialect::AnthropicApi,
        "output_config"
    ));
}

mod forwarded_effort {
    use std::sync::Arc;

    use routectl_core::wire_check::{EditSite, Leg, WireTap};
    use routectl_core::{AnthropicThinking, ChatRequest, ReasoningConfig};
    use serde_json::{Value, json};

    use super::super::thinking_carrier_tests::{body, ingress_request};

    /// `(row, request, adaptive knob, thinking type, effort on the body)`.
    type EffortRow = (
        &'static str,
        ChatRequest,
        bool,
        Option<&'static str>,
        Option<&'static str>,
    );

    fn levels(list: &[&str]) -> Arc<[String]> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn with_levels(mut req: ChatRequest, list: &[&str]) -> ChatRequest {
        req.routectl_internal.effort_levels = levels(list);
        req
    }

    fn sent(thinking: Value, effort: Option<&str>) -> ChatRequest {
        ingress_request(Some(thinking), effort, 64_000)
    }

    fn translated(effort: &str) -> ChatRequest {
        let mut req = ingress_request(None, None, 64_000);
        req.routectl_internal.anthropic_thinking = AnthropicThinking::NotCaptured;
        req.reasoning = Some(ReasoningConfig {
            effort: Some(effort.into()),
            ..Default::default()
        });
        req
    }

    /// The client's adaptive object and a canonical effort, with a sent
    /// `output_config` holding only `format`: the canonical effort is the one
    /// routectl inserts beside the adaptive shape.
    fn adaptive_with_canonical_effort_and_format_only() -> ChatRequest {
        let mut req = sent(json!({"type": "adaptive"}), None);
        req.reasoning
            .get_or_insert_with(ReasoningConfig::default)
            .effort = Some("xhigh".into());
        req.provider_extras = Some(json!({
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}}
        }));
        req
    }

    #[test]
    fn the_effort_on_the_wire_per_thinking_shape_and_operator_knob() {
        let rows: Vec<EffortRow> = vec![
            (
                "xhigh beside disabled ships as sent",
                sent(json!({"type": "disabled"}), Some("xhigh")),
                false,
                Some("disabled"),
                Some("xhigh"),
            ),
            (
                "xhigh beside disabled, knob on",
                sent(json!({"type": "disabled"}), Some("xhigh")),
                true,
                Some("disabled"),
                Some("xhigh"),
            ),
            (
                "xhigh beside enabled ships as sent",
                sent(
                    json!({"type": "enabled", "budget_tokens": 4096}),
                    Some("xhigh"),
                ),
                false,
                Some("enabled"),
                Some("xhigh"),
            ),
            (
                "none beside adaptive ships as sent",
                sent(json!({"type": "adaptive"}), Some("none")),
                false,
                Some("adaptive"),
                Some("none"),
            ),
            (
                "none beside adaptive ships as sent, knob on",
                sent(json!({"type": "adaptive"}), Some("none")),
                true,
                Some("adaptive"),
                Some("none"),
            ),
            (
                "canonical none without a carrier is disabled",
                translated("none"),
                true,
                Some("disabled"),
                None,
            ),
            (
                "explicit list clamps xhigh beside adaptive",
                with_levels(
                    sent(json!({"type": "adaptive"}), Some("xhigh")),
                    &["low", "medium", "high"],
                ),
                false,
                Some("adaptive"),
                Some("high"),
            ),
            (
                "explicit list clamps xhigh beside disabled",
                with_levels(
                    sent(json!({"type": "disabled"}), Some("xhigh")),
                    &["low", "medium", "high"],
                ),
                false,
                Some("disabled"),
                Some("high"),
            ),
            (
                "explicit list leaves none as sent",
                with_levels(
                    sent(json!({"type": "adaptive"}), Some("none")),
                    &["low", "medium", "high"],
                ),
                false,
                Some("adaptive"),
                Some("none"),
            ),
            (
                "empty list passes xhigh",
                with_levels(sent(json!({"type": "adaptive"}), Some("xhigh")), &[]),
                false,
                Some("adaptive"),
                Some("xhigh"),
            ),
            (
                "client adaptive, no effort, knob off: nothing injected",
                sent(json!({"type": "adaptive"}), None),
                false,
                Some("adaptive"),
                None,
            ),
            (
                "client adaptive, no effort, knob on: medium injected",
                sent(json!({"type": "adaptive"}), None),
                true,
                Some("adaptive"),
                Some("medium"),
            ),
            (
                "knob-converted enabled, no effort: medium injected",
                sent(json!({"type": "enabled", "budget_tokens": 4096}), None),
                true,
                Some("adaptive"),
                Some("medium"),
            ),
            (
                "canonical effort inserted beside adaptive with format only",
                adaptive_with_canonical_effort_and_format_only(),
                false,
                Some("adaptive"),
                Some("xhigh"),
            ),
            (
                "canonical effort inserted and clamped by an explicit list",
                with_levels(
                    adaptive_with_canonical_effort_and_format_only(),
                    &["low", "medium", "high"],
                ),
                false,
                Some("adaptive"),
                Some("high"),
            ),
            (
                "translated adaptive carries its canonical effort",
                translated("low"),
                true,
                Some("adaptive"),
                Some("low"),
            ),
            (
                "translated legacy shape carries no effort",
                translated("low"),
                false,
                Some("enabled"),
                None,
            ),
        ];
        for (row, req, adaptive, thinking_type, effort) in rows {
            // Act
            let wire = body(&req, adaptive);

            // Assert
            assert_eq!(
                wire.pointer("/thinking/type").and_then(Value::as_str),
                thinking_type,
                "{row}: {wire}"
            );
            assert_eq!(
                wire.pointer("/output_config/effort")
                    .and_then(Value::as_str),
                effort,
                "{row}: {wire}"
            );
        }
    }

    fn plain_turn_capture() -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../routectl-cli/tests/fixtures/driver/anthropic-api/plain-turn-01")
            .join("ingress_request.json");
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture reads"))
            .expect("fixture parses")
    }

    fn plain_turn_request() -> ChatRequest {
        let ingress = plain_turn_capture();
        let effort = ingress
            .pointer("/output_config/effort")
            .and_then(Value::as_str);
        assert_eq!(effort, Some("high"), "precondition: the capture's effort");
        let max_tokens = ingress["max_tokens"].as_u64().expect("max_tokens") as u32;
        ingress_request(Some(ingress["thinking"].clone()), effort, max_tokens)
    }

    /// The committed plain turn ships its thinking and effort as sent, and
    /// assembly declares no `output_config` edit for it.
    #[test]
    fn the_plain_turn_ships_its_effort_with_no_output_config_edit() {
        // Arrange
        let ingress = plain_turn_capture();
        let mut req = plain_turn_request();
        let tap = WireTap::admitted();
        req.routectl_internal.wire_tap = Some(tap.clone());

        // Act
        let wire = body(&req, false);

        // Assert
        let effort_edits: Vec<String> = tap
            .take_request()
            .edits(Leg::Request)
            .iter()
            .filter(|edit| edit.site() == EditSite::EffortInjection)
            .map(|edit| edit.path().to_string())
            .collect();
        assert!(effort_edits.is_empty(), "{effort_edits:?}: {wire}");
        assert_eq!(wire["thinking"], ingress["thinking"], "{wire}");
        assert_eq!(wire["output_config"], ingress["output_config"], "{wire}");
    }

    #[cfg(feature = "bedrock")]
    #[test]
    fn the_plain_turn_ships_its_effort_on_bedrock_invoke() {
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
        let req = plain_turn_request();

        // Act
        let wire = crate::bedrock::invoke::normalize_request(&cfg, &req).expect("invoke body");

        // Assert
        assert_eq!(wire["thinking"], json!({"type": "disabled"}), "{wire}");
        assert_eq!(wire["output_config"], json!({"effort": "high"}), "{wire}");
    }
}
