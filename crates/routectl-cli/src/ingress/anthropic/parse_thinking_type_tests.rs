//! An unrecognized `thinking.type` parses, collapses to the default
//! reasoning config, and announces itself with exactly one WARN.

use serde_json::json;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

const UNRECOGNIZED_WARN: &str = "unrecognized thinking.type";

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

fn unrecognized_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.contains(UNRECOGNIZED_WARN))
        .collect()
}

#[test]
fn unknown_thinking_type_parses_and_warns_once() {
    // Arrange / Act
    let events = routectl_testkit::capture_events(|| {
        let req = parse(json!({"type": "interleaved", "budget_tokens": 2048}))
            .expect("an unrecognized thinking.type must not earn a local 400");

        let reasoning = req.reasoning.expect("thinking always sets reasoning");
        assert_eq!(
            reasoning.enabled, None,
            "an unrecognized type collapses to the default reasoning config"
        );
        assert_eq!(reasoning.max_tokens, None);
    });

    // Assert
    let warns = unrecognized_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "exactly one WARN must name the unrecognized type: {events:?}"
    );
    assert_eq!(warns[0].level, tracing::Level::WARN);
    assert_eq!(
        warns[0].field("thinking_type"),
        Some(routectl_core::sanitize_detail_for_log("interleaved").as_str()),
        "the type token must render through sanitize_detail_for_log"
    );
}

#[test]
fn known_thinking_types_emit_no_unrecognized_warn() {
    // The paired positive control: the fixtures above and below share one
    // detector, so a WARN that fired on every type would look like a pass.
    for thinking in [
        json!({"type": "enabled", "budget_tokens": 2048}),
        json!({"type": "adaptive"}),
        json!({"type": "disabled"}),
    ] {
        let label = thinking.to_string();
        let events = routectl_testkit::capture_events(|| {
            parse(thinking.clone()).expect("a known thinking.type parses");
        });

        assert!(
            unrecognized_warns(&events).is_empty(),
            "a known type must emit no unrecognized-type WARN: {label} -> {events:?}"
        );
    }
}

#[test]
fn absent_thinking_type_warns_once() {
    // A `thinking` object with no `type` at all reaches the same arm: the
    // empty token is exactly as unrecognized as a wrong one.
    let events = routectl_testkit::capture_events(|| {
        parse(json!({"budget_tokens": 2048})).expect("thinking without a type must still parse");
    });

    let warns = unrecognized_warns(&events);
    assert_eq!(warns.len(), 1, "the missing token warns too: {events:?}");
    assert_eq!(warns[0].field("thinking_type"), Some(""));
}

#[test]
fn unknown_thinking_type_warn_carries_no_raw_control_characters() {
    // Makes the sanitizer claim load-bearing: `%` renders wire bytes into
    // the log line verbatim, so a type token carrying a newline plus an
    // ANSI CSI sequence would forge a whole log record.
    let events = routectl_testkit::capture_events(|| {
        parse(json!({"type": "en\nabled\u{1b}[2J"}))
            .expect("a hostile type token parses like any other unrecognized one");
    });

    let logged = unrecognized_warns(&events)
        .first()
        .and_then(|e| e.field("thinking_type"))
        .expect("the WARN carries the type token")
        .to_string();
    assert!(
        !logged.contains('\n') && !logged.contains('\u{1b}'),
        "control characters must not reach the log line: {logged:?}"
    );
}
