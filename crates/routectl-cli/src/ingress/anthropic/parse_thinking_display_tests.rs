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

/// Assert exactly one DEBUG line reported the unmodeled value, and that
/// its `thinking_display` field went through `sanitize_detail_for_log`
/// (compared against the sanitizer's own output so the assertion holds
/// under either setting of the prompt-redaction flag).
fn assert_forward_debug(events: &[routectl_testkit::CapturedEvent], raw: &str) {
    let forwarded: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("not modeled by reasoning.exclude"))
        .collect();

    assert_eq!(
        forwarded.len(),
        1,
        "exactly one DEBUG line must report the unmodeled value: {events:?}"
    );
    let event = forwarded[0];
    assert_eq!(event.level, tracing::Level::DEBUG);
    assert_eq!(
        event.field("thinking_display"),
        Some(routectl_core::sanitize_detail_for_log(raw).as_str()),
        "the value must be rendered through sanitize_detail_for_log"
    );
}

#[test]
fn thinking_display_omitted_maps_to_exclude_true() {
    // Arrange / Act
    let req = parse(json!({"type": "enabled", "budget_tokens": 2048, "display": "omitted"}))
        .expect("omitted is an accepted display value");

    // Assert
    assert_eq!(
        req.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("omitted"),
        "a modeled value still rides the carrier verbatim"
    );
    assert_eq!(
        req.reasoning.and_then(|r| r.exclude),
        Some(true),
        "display=omitted must set exclude=true"
    );
}

#[test]
fn thinking_display_updates_maps_to_exclude_true() {
    // `updates` returns empty thinking text plus a signature, the same
    // response shape as `omitted`, so it derives the same boolean.
    let req = parse(json!({"type": "enabled", "budget_tokens": 2048, "display": "updates"}))
        .expect("updates is an accepted display value");

    assert_eq!(
        req.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("updates"),
    );
    assert_eq!(
        req.reasoning.and_then(|r| r.exclude),
        Some(true),
        "display=updates must set exclude=true"
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
        req.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("summarized"),
    );
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

    assert_eq!(
        req.routectl_internal.anthropic_thinking_display, None,
        "absent display must leave the carrier unset"
    );
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
fn disabled_thinking_still_carries_the_display_string() {
    // The disabled arm drops the derived boolean, but the ingress still
    // preserves the raw value on the carrier; the disabled egress shape
    // intentionally omits `display` when it emits.
    let req = parse(json!({"type": "disabled", "display": "omitted"}))
        .expect("a legal display on the disabled shape is inert, not an error");

    assert_eq!(
        req.routectl_internal.anthropic_thinking_display.as_deref(),
        Some("omitted"),
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
fn unknown_thinking_display_value_is_forwarded_on_disabled() {
    let events = routectl_testkit::capture_events(|| {
        let req = parse(json!({"type": "disabled", "display": "verbose"}))
            .expect("an unmodeled display value forwards on every thinking type");

        let reasoning = req.reasoning.expect("reasoning set");
        assert_eq!(reasoning.enabled, Some(false));
        assert_eq!(
            reasoning.exclude, None,
            "an unmodeled value must not derive a boolean"
        );
        assert_eq!(
            req.routectl_internal.anthropic_thinking_display.as_deref(),
            Some("verbose"),
            "the carrier must hold the value verbatim on the disabled shape too"
        );
    });

    assert_forward_debug(&events, "verbose");
}

#[test]
fn non_string_thinking_display_is_rejected_on_disabled() {
    for (bad, type_name) in [
        (json!(true), "bool"),
        (json!(1), "number"),
        (json!(null), "null"),
        (json!(["omitted"]), "array"),
    ] {
        let err = parse(json!({"type": "disabled", "display": bad}))
            .expect_err("a non-string display must be rejected on the disabled shape too");

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert!(
            err.to_string().contains(&format!("got {type_name}")),
            "message must name the type it got: {err}"
        );
    }
}

#[test]
fn unknown_thinking_display_value_is_forwarded_verbatim() {
    let events = routectl_testkit::capture_events(|| {
        let req = parse(json!({
            "type": "enabled",
            "budget_tokens": 2048,
            "display": "verbose",
        }))
        .expect("an unmodeled display value must reach upstream, not earn a local 400");

        let reasoning = req.reasoning.expect("thinking sets reasoning");
        assert_eq!(
            reasoning.enabled,
            Some(true),
            "positive control: thinking on"
        );
        assert_eq!(
            reasoning.exclude, None,
            "a value the canonical boolean does not model leaves exclude unset"
        );
        assert_eq!(
            req.routectl_internal.anthropic_thinking_display.as_deref(),
            Some("verbose"),
            "the carrier must hold the value verbatim"
        );
    });

    assert_forward_debug(&events, "verbose");
}

#[test]
fn forwarded_thinking_display_debug_carries_no_raw_control_characters() {
    // Makes the sanitizer claim load-bearing: a `%`-rendered field passes
    // bytes into the log line verbatim, so a display value carrying a
    // newline plus an ANSI CSI sequence would forge a log record.
    let events = routectl_testkit::capture_events(|| {
        let req = parse(json!({
            "type": "enabled",
            "budget_tokens": 2048,
            "display": "ver\nbose\u{1b}[2J",
        }))
        .expect("a hostile display string forwards like any other unmodeled value");

        assert_eq!(
            req.routectl_internal.anthropic_thinking_display.as_deref(),
            Some("ver\nbose\u{1b}[2J"),
            "the carrier keeps the wire bytes; only the log line is sanitized"
        );
    });

    let logged = events
        .iter()
        .find(|e| e.message.contains("not modeled by reasoning.exclude"))
        .and_then(|e| e.field("thinking_display"))
        .expect("the forward DEBUG line carries the value")
        .to_string();
    assert!(
        !logged.contains('\n') && !logged.contains('\u{1b}'),
        "control characters must not reach the log line: {logged:?}"
    );
}

#[test]
fn non_string_thinking_display_is_rejected() {
    // The surviving positive control: forwarding needs a string to put on
    // the carrier, so a non-string shape still fails closed.
    for (bad, type_name) in [
        (json!(true), "bool"),
        (json!(1), "number"),
        (json!(null), "null"),
        (json!(["omitted"]), "array"),
    ] {
        let err = parse(json!({
            "type": "enabled",
            "budget_tokens": 2048,
            "display": bad,
        }))
        .expect_err("a non-string display must be rejected");

        assert!(matches!(err, Error::Validation(_)), "got {err:?}");
        assert_eq!(
            err.to_string(),
            format!(
                "validation: anthropic ingress: thinking.display must be a string, got {type_name}"
            ),
            "message must name the field and the type it got"
        );
    }
}
