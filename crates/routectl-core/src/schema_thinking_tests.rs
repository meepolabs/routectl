//! Tests for the native Anthropic thinking carrier and the reasoning-off
//! rule on [`ReasoningConfig`].

use serde_json::json;

use super::*;

fn sent_enabled() -> AnthropicThinking {
    AnthropicThinking::Sent(
        ThinkingObject::from_client(json!({"type": "enabled", "budget_tokens": 1024}))
            .expect("object"),
    )
}

#[test]
fn default_carrier_is_not_captured_and_the_three_states_differ() {
    let default_internal = RoutectlInternal::default();

    assert_eq!(
        default_internal.anthropic_thinking,
        AnthropicThinking::NotCaptured
    );
    assert_ne!(AnthropicThinking::NotCaptured, AnthropicThinking::SentNone);
    assert_ne!(AnthropicThinking::NotCaptured, sent_enabled());
    assert_ne!(AnthropicThinking::SentNone, sent_enabled());
}

#[test]
fn cloning_a_request_shares_the_sent_thinking_allocation() {
    let mut req = ChatRequest::default();
    req.routectl_internal.anthropic_thinking = sent_enabled();

    let cloned = req.clone();

    let (AnthropicThinking::Sent(original), AnthropicThinking::Sent(copy)) = (
        &req.routectl_internal.anthropic_thinking,
        &cloned.routectl_internal.anthropic_thinking,
    ) else {
        panic!("both requests must carry Sent");
    };
    assert!(original.shares_allocation_with(copy));
}

#[test]
fn is_off_holds_for_disabled_or_none_effort_only() {
    let cfg = |enabled: Option<bool>, effort: Option<&str>| ReasoningConfig {
        enabled,
        effort: effort.map(str::to_owned),
        ..ReasoningConfig::default()
    };
    let rows = [
        ("enabled=false", cfg(Some(false), None), true),
        ("effort=none", cfg(None, Some("none")), true),
        (
            "enabled=true+effort=none",
            cfg(Some(true), Some("none")),
            true,
        ),
        ("effort=low", cfg(None, Some("low")), false),
        ("enabled=true", cfg(Some(true), None), false),
        ("default", ReasoningConfig::default(), false),
    ];

    for (name, config, expected) in rows {
        assert_eq!(config.is_off(), expected, "row {name}");
    }
}

#[test]
fn is_off_effort_matches_exactly_none() {
    let rows = [("none", true), ("NONE", false), ("", false), ("low", false)];

    for (token, expected) in rows {
        assert_eq!(
            ReasoningConfig::is_off_effort(token),
            expected,
            "token {token:?}"
        );
    }
}
