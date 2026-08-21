//! `reasoning.exclude` -> Anthropic `thinking.display` egress mapping.
//!
//! The assertions read the SERIALIZED body, not the enum, because
//! `skip_serializing_if` is the mechanism that keeps an absent display
//! off the wire.

use routectl_core::{ChatRequest, ReasoningConfig, RoutectlInternal};
use serde_json::{Value, json};

use super::build_thinking;

fn thinking_body(exclude: Option<bool>, adaptive: bool) -> Value {
    let req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            exclude,
            ..Default::default()
        }),
        routectl_internal: RoutectlInternal::default(),
        ..Default::default()
    };
    let thinking = build_thinking(&req, adaptive).expect("effort=high activates thinking");
    serde_json::to_value(&thinking).expect("thinking serializes")
}

#[test]
fn enabled_thinking_serializes_display_omitted_for_exclude_true() {
    let body = thinking_body(Some(true), false);

    assert_eq!(body["type"], "enabled");
    assert_eq!(body["display"], "omitted");
}

#[test]
fn enabled_thinking_serializes_display_summarized_for_exclude_false() {
    let body = thinking_body(Some(false), false);

    assert_eq!(body["type"], "enabled");
    assert_eq!(body["display"], "summarized");
}

#[test]
fn enabled_thinking_omits_the_display_key_when_exclude_is_none() {
    // Load-bearing negative. Its positive controls are the two tests
    // above (same builder, same input, display present) -- so a builder
    // that stopped emitting display at all could not make this pass.
    let body = thinking_body(None, false);

    assert_eq!(body["type"], "enabled");
    assert!(
        body.get("display").is_none(),
        "an absent exclude must leave no display key at all; got {body}"
    );
    assert!(
        body.get("budget_tokens").is_some(),
        "positive control: the enabled shape still carries a budget"
    );
}

#[test]
fn adaptive_thinking_serializes_display_omitted_for_exclude_true() {
    let body = thinking_body(Some(true), true);

    assert_eq!(body["type"], "adaptive");
    assert_eq!(body["display"], "omitted");
    assert!(
        body.get("budget_tokens").is_none(),
        "the adaptive shape has no budget field"
    );
}

#[test]
fn adaptive_thinking_serializes_display_summarized_for_exclude_false() {
    let body = thinking_body(Some(false), true);

    assert_eq!(body["type"], "adaptive");
    assert_eq!(body["display"], "summarized");
}

#[test]
fn adaptive_thinking_omits_the_display_key_when_exclude_is_none() {
    let body = thinking_body(None, true);

    assert_eq!(
        body,
        json!({"type": "adaptive"}),
        "the adaptive shape with no display is exactly the bare discriminator"
    );
}

#[test]
fn disabled_thinking_never_carries_display() {
    // exclude=true with reasoning off must not smuggle display onto the
    // disabled shape -- display is stamped only where thinking is built.
    let req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            enabled: Some(false),
            exclude: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    let thinking = build_thinking(&req, false).expect("enabled=false yields the disabled shape");
    let body = serde_json::to_value(&thinking).expect("thinking serializes");

    assert_eq!(body, json!({"type": "disabled"}));
}

#[test]
fn exclude_alone_does_not_activate_thinking() {
    // `exclude` is a display modifier, never an activation signal.
    let req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            exclude: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };

    assert!(
        build_thinking(&req, false).is_none(),
        "exclude with no enabled/budget/effort must not turn thinking on"
    );
    assert!(
        build_thinking(&req, true).is_none(),
        "same on the adaptive path"
    );
}
