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

/// A caller's `output_config.effort = "max"` carried in provider_extras
/// flows through verbatim.
#[test]
fn provider_extras_effort_passes_through() {
    use serde_json::json;

    // Arrange
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
            "output_config": {"effort": "max"}
        })),
        ..Default::default()
    };

    // Act
    let body = normalize("test", &req, false, None, false, true).expect("normalize must succeed");

    // Assert
    let oc = body
        .get("output_config")
        .expect("output_config present on adaptive path");
    assert_eq!(
        oc["effort"], "max",
        "provider_extras output_config.effort must pass through unchanged; got: {oc}"
    );
}

#[test]
fn output_config_is_not_routectl_managed() {
    // Pinning this invariant: output_config must remain a non-managed
    // key so provider_extras-carried sub-fields like
    // `output_config.format` flow through verbatim.
    assert!(!crate::managed_keys::is_managed_key(
        crate::managed_keys::Dialect::AnthropicApi,
        "output_config"
    ));
}

mod forwarded_effort {
    use routectl_core::wire_check::{EditSite, Leg, WireTap};
    use routectl_core::{AnthropicThinking, ChatRequest, ReasoningConfig};
    use serde_json::{Value, json};

    use super::super::thinking_carrier_tests::{body, ingress_request};

    /// `(row, request, thinking type, effort on the body)`.
    type EffortRow = (
        &'static str,
        ChatRequest,
        Option<&'static str>,
        Option<&'static str>,
    );

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
    fn the_effort_on_the_wire_per_thinking_shape() {
        let rows: Vec<EffortRow> = vec![
            (
                "xhigh beside disabled ships as sent",
                sent(json!({"type": "disabled"}), Some("xhigh")),
                Some("disabled"),
                Some("xhigh"),
            ),
            (
                "xhigh beside enabled ships as sent",
                sent(
                    json!({"type": "enabled", "budget_tokens": 4096}),
                    Some("xhigh"),
                ),
                Some("enabled"),
                Some("xhigh"),
            ),
            (
                "none beside adaptive ships as sent",
                sent(json!({"type": "adaptive"}), Some("none")),
                Some("adaptive"),
                Some("none"),
            ),
            (
                "canonical none without a carrier is disabled",
                translated("none"),
                Some("disabled"),
                None,
            ),
            (
                "xhigh beside adaptive ships as sent",
                sent(json!({"type": "adaptive"}), Some("xhigh")),
                Some("adaptive"),
                Some("xhigh"),
            ),
            (
                "client adaptive, no effort: nothing injected",
                sent(json!({"type": "adaptive"}), None),
                Some("adaptive"),
                None,
            ),
            (
                "canonical effort inserted beside adaptive with format only",
                adaptive_with_canonical_effort_and_format_only(),
                Some("adaptive"),
                Some("xhigh"),
            ),
            (
                "translated legacy shape carries no effort",
                translated("low"),
                Some("enabled"),
                None,
            ),
        ];
        for (row, req, thinking_type, effort) in rows {
            // Act
            let wire = body(&req);

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
        let wire = body(&req);

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
        };
        let req = plain_turn_request();

        // Act
        let wire = crate::bedrock::invoke::normalize_request(&cfg, &req).expect("invoke body");

        // Assert
        assert_eq!(wire["thinking"], json!({"type": "disabled"}), "{wire}");
        assert_eq!(wire["output_config"], json!({"effort": "high"}), "{wire}");
    }
}
