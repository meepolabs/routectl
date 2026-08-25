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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

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
    let body = normalize("test", &req, false, &[], false, None, false, true)
        .expect("normalize must succeed");

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
/// sub-key (structured-output) passes through. derive_effort clamps
/// "max" -> "high" on the typed struct, but merge_provider_extras
/// then overwrites the clamped wire value with the raw caller
/// value. Without a re-clamp on the adaptive branch of
/// reconcile_output_config_effort, the operator's effort_levels
/// cap is silently bypassed.
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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

    // Assert: effort clamped, format preserved verbatim.
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(oc["effort"], "high", "effort must clamp; got: {oc}");
    assert_eq!(oc["format"]["type"], "json_schema");
    assert_eq!(oc["format"]["schema"]["required"][0], "x");
}

/// An adaptive-thinking request whose provider_extras override the generated
/// `output_config.effort` with "none" is a reasoning-OFF request. The body
/// must not ship `thinking: {type: "adaptive"}` alongside an effort of "none":
/// the effort is omitted and thinking is reconciled to the disabled form, so
/// the caller is not billed for thinking it declined.
#[test]
fn adaptive_provider_extras_none_effort_omits_effort_and_disables_thinking() {
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
    let body = normalize("test", &req, true, &[], false, None, false, true)
        .expect("normalize must succeed");

    // Assert: no effort survives, and thinking is the explicit disable form.
    assert!(
        body.get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_none(),
        "output_config.effort=none must be omitted, not shipped; got: {body}"
    );
    assert_eq!(
        body["thinking"]["type"], "disabled",
        "an effort of none must reconcile thinking to disabled, not leave it adaptive; got: {body}"
    );
}

#[test]
fn output_config_is_not_routectl_managed() {
    // Pinning this invariant: output_config must remain a non-managed
    // key so provider_extras-carried sub-fields like
    // `output_config.format` flow through verbatim. The adaptive-branch
    // re-clamp at reconcile_output_config_effort relies on output_config
    // surviving merge_provider_extras intact.
    assert!(!is_routectl_managed_key("output_config"));
}
