//! The ingress stores the client's `thinking` object, minus `display`, on
//! the native carrier for every type, and marks a request without one.

use serde_json::{Value, json};

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

fn parse(thinking: Option<Value>) -> ChatRequest {
    let mut body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 4096,
    });
    if let Some(t) = thinking {
        body["thinking"] = t;
    }
    AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("request should parse")
}

/// One row: its name, the client's `thinking`, and the `display` the
/// display carrier must hold afterwards.
type CarrierRow = (&'static str, Value, Option<&'static str>);

#[test]
fn every_thinking_type_is_stored_verbatim_minus_display() {
    let rows: [CarrierRow; 5] = [
        (
            "enabled with budget and display",
            json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"}),
            Some("summarized"),
        ),
        (
            "adaptive with display",
            json!({"type": "adaptive", "display": "omitted"}),
            Some("omitted"),
        ),
        ("disabled", json!({"type": "disabled"}), None),
        ("between_tools", json!({"type": "between_tools"}), None),
        (
            "unknown type with an extra member",
            json!({"type": "future_mode", "knob": {"display": "nested", "n": 3}}),
            None,
        ),
    ];

    for (name, client, display) in rows {
        let req = parse(Some(client.clone()));

        let mut expected = client.as_object().expect("rows are objects").clone();
        expected.remove("display");
        let AnthropicThinking::Sent(stored) = &req.routectl_internal.anthropic_thinking else {
            panic!(
                "{name}: expected Sent, got {:?}",
                req.routectl_internal.anthropic_thinking
            );
        };
        assert_eq!(
            serde_json::to_vec(stored.as_object()).expect("serializes"),
            serde_json::to_vec(&expected).expect("serializes"),
            "{name}: stored object must be the client's bytes minus display",
        );
        assert_eq!(
            req.routectl_internal.anthropic_thinking_display.as_deref(),
            display,
            "{name}: display rides its own carrier",
        );
    }
}

#[test]
fn absent_thinking_is_marked_sent_none() {
    let req = parse(None);

    assert_eq!(
        req.routectl_internal.anthropic_thinking,
        AnthropicThinking::SentNone
    );
    assert!(req.reasoning.is_none(), "no thinking derives no reasoning");
}

#[test]
fn non_object_thinking_is_left_uncaptured() {
    for (name, client) in [("null", Value::Null), ("string", json!("enabled"))] {
        let req = parse(Some(client));

        assert_eq!(
            req.routectl_internal.anthropic_thinking,
            AnthropicThinking::NotCaptured,
            "{name}: no object to forward, and the client did not send none",
        );
        assert_eq!(
            serde_json::to_value(&req.reasoning).expect("serializes"),
            serde_json::to_value(Some(ReasoningConfig::default())).expect("serializes"),
            "{name}: canonical intent is unchanged",
        );
    }
}
