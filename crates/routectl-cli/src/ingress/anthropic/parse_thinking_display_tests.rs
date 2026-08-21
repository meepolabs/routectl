//! `thinking.display` -> canonical `ReasoningConfig.exclude` translation.

use serde_json::json;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

fn parse(thinking: serde_json::Value) -> Result<ChatRequest> {
    AnthropicIngress.parse_request_value(
        &HeaderMap::new(),
        json!({
            "model": "claude-opus-4-7",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 4096,
            "thinking": thinking,
        }),
    )
}

#[test]
fn thinking_display_omitted_maps_to_exclude_true() {
    // Arrange / Act
    let req = parse(json!({"type": "enabled", "budget_tokens": 2048, "display": "omitted"}))
        .expect("omitted is an accepted display value");

    // Assert
    assert_eq!(
        req.reasoning.and_then(|r| r.exclude),
        Some(true),
        "display=omitted must set exclude=true"
    );
}

#[test]
fn thinking_display_summarized_maps_to_exclude_false() {
    let req = parse(json!({
        "type": "enabled",
        "budget_tokens": 2048,
        "display": "summarized",
    }))
    .expect("summarized is an accepted display value");

    assert_eq!(
        req.reasoning.and_then(|r| r.exclude),
        Some(false),
        "display=summarized must set exclude=false"
    );
}

#[test]
fn absent_thinking_display_leaves_exclude_none() {
    // The load-bearing negative: no display key must NOT materialize an
    // exclude, because Anthropic's display default is model-dependent.
    let req = parse(json!({"type": "enabled", "budget_tokens": 2048}))
        .expect("thinking without display is valid");

    let reasoning = req.reasoning.expect("thinking sets reasoning");
    assert_eq!(
        reasoning.enabled,
        Some(true),
        "positive control: thinking on"
    );
    assert_eq!(
        reasoning.exclude, None,
        "absent display must leave exclude unset"
    );
}

#[test]
fn adaptive_thinking_display_omitted_maps_to_exclude_true() {
    let req = parse(json!({"type": "adaptive", "display": "omitted"}))
        .expect("display is accepted on the adaptive shape too");

    let reasoning = req.reasoning.expect("adaptive thinking sets reasoning");
    assert_eq!(reasoning.enabled, Some(true));
    assert_eq!(reasoning.exclude, Some(true));
}

#[test]
fn adaptive_thinking_without_display_leaves_exclude_none() {
    let req = parse(json!({"type": "adaptive"})).expect("adaptive without display is valid");

    assert_eq!(req.reasoning.expect("reasoning set").exclude, None);
}

#[test]
fn disabled_thinking_ignores_valid_display() {
    // The disabled shape has no thinking to display; a legal display
    // value is accepted and then ignored.
    let req = parse(json!({"type": "disabled", "display": "omitted"}))
        .expect("a legal display on the disabled shape is inert, not an error");

    let reasoning = req.reasoning.expect("reasoning set");
    assert_eq!(reasoning.enabled, Some(false));
    assert_eq!(
        reasoning.exclude, None,
        "the disabled arm must not honor display"
    );
}

#[test]
fn disabled_thinking_without_display_is_accepted() {
    let req = parse(json!({"type": "disabled"})).expect("disabled without display is valid");

    let reasoning = req.reasoning.expect("reasoning set");
    assert_eq!(reasoning.enabled, Some(false));
    assert_eq!(reasoning.exclude, None);
}

#[test]
fn unknown_thinking_display_value_is_rejected_on_disabled() {
    let err = parse(json!({"type": "disabled", "display": "verbose"}))
        .expect_err("an unrecognized display value must be rejected on every thinking type");

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("summarized") && msg.contains("omitted"),
        "message must name the legal values: {msg}"
    );
    assert!(
        !msg.contains("verbose"),
        "message must not echo the rejected value: {msg}"
    );
}

#[test]
fn non_string_thinking_display_is_rejected_on_disabled() {
    for bad in [json!(true), json!(1), json!(null), json!(["omitted"])] {
        let err = parse(json!({"type": "disabled", "display": bad}))
            .expect_err("a non-string display must be rejected on the disabled shape too");

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    }
}

#[test]
fn unknown_thinking_display_value_is_rejected() {
    let err = parse(json!({
        "type": "enabled",
        "budget_tokens": 2048,
        "display": "verbose",
    }))
    .expect_err("an unrecognized display value must not reach upstream");

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    let msg = err.to_string();
    assert!(
        msg.contains("summarized"),
        "message must name summarized: {msg}"
    );
    assert!(msg.contains("omitted"), "message must name omitted: {msg}");
    assert!(
        !msg.contains("verbose"),
        "message must not echo the rejected value: {msg}"
    );
}

#[test]
fn non_string_thinking_display_is_rejected() {
    for bad in [json!(true), json!(1), json!(null), json!(["omitted"])] {
        let err = parse(json!({
            "type": "enabled",
            "budget_tokens": 2048,
            "display": bad,
        }))
        .expect_err("a non-string display must be rejected");

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        let msg = err.to_string();
        assert_eq!(
            msg,
            "validation: anthropic ingress: thinking.display must be one of \
             \"summarized\" or \"omitted\"",
            "message must name the field and its legal values only"
        );
    }
}
