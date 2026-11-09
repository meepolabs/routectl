//! The Converse bag's `thinking` and `output_config.effort`, and the
//! `inferenceConfig` sampling clamp, for requests an Anthropic client sent:
//! the client's thinking object and effort ride the bag as sent, a forced
//! tool choice strips only active thinking, and the clamp keys off the
//! thinking type the final bag ships.
//!
//! Requests are built the way the Anthropic ingress builds them, through the
//! anthropic-api egress's own test builder.

use routectl_core::wire_check::{EditSite, Leg, WireTap};
use routectl_core::{AnthropicThinking, ChatRequest, CustomTool, ReasoningConfig, ToolDef};
use serde_json::{Value, json};

use super::super::normalize_request;
use crate::anthropic_api::request::thinking_carrier_tests::ingress_request;
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const CALLER_TEMPERATURE: f64 = 0.3;

fn cfg() -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-opus-5-5".into(),
        api_shape: BedrockApiShape::Converse,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

fn converse_body(req: &routectl_core::ChatRequest) -> Value {
    normalize_request(&cfg(), req).expect("translate")
}

fn bag(body: &Value) -> &Value {
    &body["additionalModelRequestFields"]
}

fn bag_effort(body: &Value) -> Option<&str> {
    bag(body)
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
}

fn driver_fixture(case: &str, file: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../routectl-cli/tests/fixtures/driver/anthropic-api")
        .join(case)
        .join(file);
    let text = std::fs::read_to_string(&path).expect("driver fixture must read");
    serde_json::from_str(&text).expect("driver fixture must parse")
}

/// Every type is sent with a `display` so the row also proves the display
/// comes back from its own carrier. Each object differs from what the
/// canonical translation would build (a budget it would clamp or drop, a
/// type it cannot express), so only the forwarding path produces it.
#[test]
fn the_client_thinking_object_reaches_the_bag_as_sent() {
    for thinking in [
        json!({"type": "enabled", "budget_tokens": 1500, "display": "summarized"}),
        json!({"type": "adaptive", "display": "omitted"}),
        json!({"type": "disabled", "display": "summarized"}),
        json!({"type": "between_tools", "display": "omitted"}),
        json!({"type": "future_mode", "depth": 3, "display": "future_display"}),
    ] {
        // Arrange
        let req = ingress_request(Some(thinking.clone()), None, 1200);

        // Act
        let body = converse_body(&req);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
    }
}

/// The committed plain-turn capture: disabled thinking beside a positive
/// effort, the shape a real client sends for a turn that should not think.
#[test]
fn the_plain_turn_capture_keeps_its_effort_beside_disabled_thinking() {
    // Arrange
    let ingress = driver_fixture(
        "plain-turn-01__claude-code__anthropic-api",
        "ingress_request.json",
    );
    let effort = ingress
        .pointer("/output_config/effort")
        .and_then(Value::as_str);
    assert!(
        effort.is_some(),
        "precondition: the capture carries an effort"
    );
    let max_tokens = ingress["max_tokens"].as_u64().expect("max_tokens") as u32;
    let req = ingress_request(Some(ingress["thinking"].clone()), effort, max_tokens);

    // Act
    let body = converse_body(&req);

    // Assert
    assert_eq!(bag(&body)["thinking"], ingress["thinking"], "{body}");
    assert_eq!(bag_effort(&body), effort, "{body}");
}

/// `(row, thinking sent, effort sent, effort on the bag)`.
type EffortRow<'a> = (&'a str, Option<Value>, Option<&'a str>, Option<&'a str>);

#[test]
fn the_client_effort_reaches_the_bag_as_sent() {
    let rows: &[EffortRow] = &[
        (
            "adaptive, no list",
            Some(json!({"type": "adaptive"})),
            Some("xhigh"),
            Some("xhigh"),
        ),
        (
            "enabled, no list",
            Some(json!({"type": "enabled", "budget_tokens": 4096})),
            Some("max"),
            Some("max"),
        ),
        (
            "disabled, no list",
            Some(json!({"type": "disabled"})),
            Some("high"),
            Some("high"),
        ),
        (
            "adaptive, none",
            Some(json!({"type": "adaptive"})),
            Some("none"),
            Some("none"),
        ),
        (
            "adaptive, no effort",
            Some(json!({"type": "adaptive"})),
            None,
            None,
        ),
        ("no thinking, effort", None, Some("low"), Some("low")),
    ];
    for (row, thinking, effort, expected) in rows {
        // Arrange
        let req = ingress_request(thinking.clone(), *effort, 64_000);

        // Act
        let body = converse_body(&req);

        // Assert
        assert_eq!(bag_effort(&body), *expected, "{row}: {body}");
        assert_eq!(
            bag(&body).get("thinking"),
            thinking.as_ref(),
            "{row}: thinking rides as sent: {body}"
        );
    }
}

/// A probe-sized request: the canonical translation would emit no thinking
/// here, but the client's adaptive object ships, so the clamp must read the
/// bag.
#[test]
fn an_adaptive_carrier_on_a_legacy_lane_pins_temperature() {
    // Arrange
    let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 512);
    req.temperature = Some(CALLER_TEMPERATURE);

    // Act
    let body = converse_body(&req);

    // Assert
    assert_eq!(bag(&body)["thinking"]["type"], "adaptive", "{body}");
    assert_eq!(body["inferenceConfig"]["temperature"], 1.0, "{body}");
}

#[test]
fn adaptive_thinking_without_a_client_temperature_ships_no_temperature() {
    // Arrange
    let req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 64_000);

    // Act
    let body = converse_body(&req);

    // Assert
    assert_eq!(bag(&body)["thinking"]["type"], "adaptive", "{body}");
    let inference = body["inferenceConfig"]
        .as_object()
        .expect("max_tokens keeps inferenceConfig on the body");
    assert!(!inference.contains_key("temperature"), "{body}");
    assert!(!inference.contains_key("topP"), "{body}");
}

#[test]
fn inactive_thinking_types_keep_the_caller_temperature() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode"}),
    ] {
        // Arrange
        let mut req = ingress_request(Some(thinking.clone()), Some("high"), 64_000);
        req.temperature = Some(CALLER_TEMPERATURE);

        // Act
        let body = converse_body(&req);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
        assert_eq!(
            body["inferenceConfig"]["temperature"], CALLER_TEMPERATURE,
            "{thinking}: {body}"
        );
    }
}

fn forcing_tool_use(mut req: routectl_core::ChatRequest) -> routectl_core::ChatRequest {
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "calc".into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);
    req.tool_choice = Some(json!("required"));
    req
}

/// Forced tool use strips active thinking from the bag, declares the strip
/// once, and gives the caller's sampling back.
#[test]
fn forced_tool_use_strips_active_thinking_once_and_restores_the_caller_sampling() {
    for thinking in [
        json!({"type": "adaptive"}),
        json!({"type": "enabled", "budget_tokens": 4096}),
    ] {
        // Arrange
        let mut req = forcing_tool_use(ingress_request(Some(thinking.clone()), None, 64_000));
        req.top_p = Some(0.9);

        // Act
        let (body, edits) = declared(req, &[EditSite::ThinkingStripForcedToolUse]);

        // Assert
        assert!(bag(&body).get("thinking").is_none(), "{thinking}: {body}");
        assert_eq!(
            edits,
            vec![(EditSite::ThinkingStripForcedToolUse, "/thinking".into())],
            "{thinking}"
        );
        let inference = &body["inferenceConfig"];
        assert!(inference.get("temperature").is_none(), "{thinking}: {body}");
        assert_eq!(inference["topP"], 0.9, "{thinking}: {body}");
    }
}

const LEARNED_CEILING: u32 = 8192;

/// A request with no `max_tokens` under a router ceiling of
/// [`LEARNED_CEILING`], its legacy thinking translated from canonical
/// reasoning so the budget is sized from that ceiling.
fn legacy_thinking_without_max_tokens() -> ChatRequest {
    let mut req = ingress_request(None, None, 0);
    req.max_tokens = None;
    req.routectl_internal.anthropic_thinking = AnthropicThinking::NotCaptured;
    req.routectl_internal.max_output_tokens = LEARNED_CEILING;
    req.reasoning = Some(ReasoningConfig {
        enabled: Some(true),
        ..Default::default()
    });
    req
}

/// `(row, request, thinking type shipped, maxTokens sent, declared edits)`.
type CeilingRow = (
    &'static str,
    ChatRequest,
    Option<&'static str>,
    Option<u64>,
    Vec<(EditSite, String)>,
);

/// Legacy thinking sized from the resolved ceiling ships that ceiling as
/// `maxTokens`, declared as routectl's own value; any other shape forwards
/// the caller's limit, absent or not, and declares no `maxTokens` edit.
#[test]
fn legacy_thinking_without_max_tokens_sends_the_resolved_ceiling() {
    let adaptive = {
        let mut req = ingress_request(Some(json!({"type": "adaptive"})), Some("high"), 0);
        req.max_tokens = None;
        req.routectl_internal.max_output_tokens = LEARNED_CEILING;
        req
    };
    let rows: Vec<CeilingRow> = vec![
        (
            "legacy thinking under the ceiling",
            legacy_thinking_without_max_tokens(),
            Some("enabled"),
            Some(u64::from(LEARNED_CEILING)),
            vec![(EditSite::RequiredFieldDefault, "/max_tokens".into())],
        ),
        (
            "adaptive thinking",
            adaptive,
            Some("adaptive"),
            None,
            Vec::new(),
        ),
        (
            "caller max_tokens",
            ChatRequest {
                max_tokens: Some(2048),
                ..legacy_thinking_without_max_tokens()
            },
            Some("enabled"),
            Some(2048),
            Vec::new(),
        ),
        (
            "forced tool use strips the thinking",
            forcing_tool_use(legacy_thinking_without_max_tokens()),
            None,
            None,
            vec![(EditSite::ThinkingStripForcedToolUse, "/thinking".into())],
        ),
    ];
    for (row, req, expected_thinking, expected, expected_edits) in rows {
        // Act
        let (body, edits) = declared(
            req,
            &[
                EditSite::RequiredFieldDefault,
                EditSite::ThinkingStripForcedToolUse,
            ],
        );

        // Assert
        let sent = body
            .pointer("/inferenceConfig/maxTokens")
            .and_then(Value::as_u64);
        assert_eq!(sent, expected, "{row}: {body}");
        assert_eq!(edits, expected_edits, "{row}");
        let thinking_type = bag(&body).pointer("/thinking/type").and_then(Value::as_str);
        assert_eq!(thinking_type, expected_thinking, "{row}: {body}");
        if thinking_type == Some("enabled") {
            let budget = bag(&body)["thinking"]["budget_tokens"]
                .as_u64()
                .expect("legacy thinking carries a budget");
            let window = sent.expect("a legacy budget ships beside maxTokens");
            assert!(budget < window, "{row}: budget {budget} vs {window}");
        }
    }
}

/// With no thinking in the bag a forced tool choice has nothing to strip:
/// the tool choice ships and the egress declares no edit at all.
#[test]
fn forced_tool_choice_without_thinking_declares_nothing() {
    // Arrange
    let req = forcing_tool_use(ingress_request(None, None, 64_000));

    // Act
    let (body, edits) = declared_all(req);

    // Assert
    assert!(bag(&body).get("thinking").is_none(), "{body}");
    assert!(
        body["toolConfig"].get("toolChoice").is_some(),
        "the forced choice ships: {body}"
    );
    assert!(edits.is_empty(), "{edits:?}");
}

/// Inactive and unknown thinking types spend no reasoning budget, so they
/// ride beside a forced tool choice with the client's effort and sampling,
/// and nothing is declared.
#[test]
fn forced_tool_use_keeps_inactive_thinking_with_the_client_effort() {
    for thinking in [
        json!({"type": "disabled"}),
        json!({"type": "between_tools"}),
        json!({"type": "future_mode"}),
    ] {
        // Arrange
        let mut req = forcing_tool_use(ingress_request(
            Some(thinking.clone()),
            Some("high"),
            64_000,
        ));
        req.temperature = Some(CALLER_TEMPERATURE);

        // Act
        let (body, edits) = declared(req, &[EditSite::ThinkingStripForcedToolUse]);

        // Assert
        assert_eq!(bag(&body)["thinking"], thinking, "{body}");
        assert_eq!(bag_effort(&body), Some("high"), "{thinking}: {body}");
        assert_eq!(
            body["inferenceConfig"]["temperature"], CALLER_TEMPERATURE,
            "{thinking}: {body}"
        );
        assert!(edits.is_empty(), "{thinking}: {edits:?}");
    }
}

/// `(row, thinking sent, effort sent, effort on the bag)`.
type StrippedEffortRow<'a> = (&'a str, Value, Option<&'a str>, Option<&'a str>);

/// The strip takes `output_config.effort` with it only when routectl
/// composed the effort; the client's own effort stays.
#[test]
fn forced_tool_use_drops_only_a_routectl_composed_effort() {
    let rows: &[StrippedEffortRow] = &[
        (
            "client effort beside adaptive",
            json!({"type": "adaptive"}),
            Some("xhigh"),
            Some("xhigh"),
        ),
        (
            "client effort beside enabled",
            json!({"type": "enabled", "budget_tokens": 4096}),
            Some("low"),
            Some("low"),
        ),
    ];
    for (row, thinking, effort, expected) in rows {
        // Arrange
        let req = forcing_tool_use(ingress_request(Some(thinking.clone()), *effort, 64_000));

        // Act
        let body = converse_body(&req);

        // Assert
        assert!(bag(&body).get("thinking").is_none(), "{row}: {body}");
        assert_eq!(bag_effort(&body), *expected, "{row}: {body}");
    }
}

fn declared(
    req: routectl_core::ChatRequest,
    sites: &[EditSite],
) -> (Value, Vec<(EditSite, String)>) {
    let (body, edits) = declared_all(req);
    let edits = edits
        .into_iter()
        .filter(|(site, _)| sites.contains(site))
        .collect();
    (body, edits)
}

fn declared_all(req: routectl_core::ChatRequest) -> (Value, Vec<(EditSite, String)>) {
    let mut req = req;
    let tap = WireTap::admitted();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let body = converse_body(&req);
    let edits = tap
        .take_request()
        .edits(Leg::Request)
        .iter()
        .map(|edit| (edit.site(), edit.path().to_string()))
        .collect();
    (body, edits)
}
