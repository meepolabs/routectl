//! `reasoning.exclude` -> Anthropic `thinking.display` egress mapping,
//! and the `routectl_internal.anthropic_thinking_display` carrier that
//! overrides it.
//!
//! The assertions read the SERIALIZED body, not the enum, because
//! `skip_serializing_if` is the mechanism that keeps an absent display
//! off the wire.

use routectl_core::{ChatRequest, ReasoningConfig};
use serde_json::{Value, json};

use super::build_thinking;

fn thinking_body(exclude: Option<bool>, adaptive: bool) -> Value {
    thinking_body_with_carrier(exclude, None, adaptive)
}

fn thinking_body_with_carrier(
    exclude: Option<bool>,
    carrier: Option<&str>,
    adaptive: bool,
) -> Value {
    let mut req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            effort: Some("high".into()),
            exclude,
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = carrier.map(str::to_string);
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

// -- carrier ------------------------------------------------------------

#[test]
fn enabled_thinking_forwards_the_carrier_string_verbatim() {
    // "updates" is a value the canonical `exclude` boolean cannot
    // express, so only a verbatim carrier forward can produce it.
    let body = thinking_body_with_carrier(None, Some("updates"), false);

    assert_eq!(body["type"], "enabled");
    assert_eq!(body["display"], "updates");
}

#[test]
fn adaptive_thinking_forwards_the_carrier_string_verbatim() {
    let body = thinking_body_with_carrier(None, Some("updates"), true);

    assert_eq!(body["type"], "adaptive");
    assert_eq!(body["display"], "updates");
    assert!(
        body.get("budget_tokens").is_none(),
        "the adaptive shape has no budget field"
    );
}

#[test]
fn carrier_wins_over_a_conflicting_exclude_derivation() {
    // exclude=true derives "omitted"; the carrier must override it on
    // both wire shapes, because the carrier holds what the client sent
    // and the boolean is only this hub's lossy model of it.
    let enabled = thinking_body_with_carrier(Some(true), Some("summarized"), false);
    assert_eq!(enabled["display"], "summarized");

    let adaptive = thinking_body_with_carrier(Some(false), Some("omitted"), true);
    assert_eq!(adaptive["display"], "omitted");
}

#[test]
fn absent_carrier_falls_back_to_the_exclude_derivation() {
    // Positive control for the fallback: same builder, carrier absent,
    // so a build_thinking that read ONLY the carrier could not pass.
    let body = thinking_body_with_carrier(Some(true), None, false);

    assert_eq!(body["display"], "omitted");
}

#[test]
fn carrier_alone_does_not_activate_thinking() {
    // The carrier is a display modifier, never an activation signal --
    // same contract `exclude` has.
    let mut req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig::default()),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("updates".into());

    assert!(
        build_thinking(&req, false).is_none(),
        "a carrier with no enabled/budget/effort must not turn thinking on"
    );
    assert!(
        build_thinking(&req, true).is_none(),
        "same on the adaptive path"
    );
}

#[test]
fn disabled_thinking_never_carries_the_carrier_string() {
    let mut req = ChatRequest {
        max_tokens: Some(100_000),
        reasoning: Some(ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("updates".into());
    let thinking = build_thinking(&req, false).expect("enabled=false yields the disabled shape");
    let body = serde_json::to_value(&thinking).expect("thinking serializes");

    assert_eq!(body, json!({"type": "disabled"}));
}
